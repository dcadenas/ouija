use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use nostr_sdk::prelude::*;
use tokio::sync::RwLock;

use crate::protocol::WireMessage;
use crate::state::AppState;
use crate::transport::Transport;

fn opencode_binding_for_backend_session(
    is_http_api: bool,
    backend_session_id: Option<&str>,
) -> Option<crate::daemon_protocol::OpenCodeBinding> {
    if !is_http_api {
        None
    } else if backend_session_id.is_some() {
        Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged)
    } else {
        Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted)
    }
}

fn opencode_binding_for_restart_session(
    is_http_api: bool,
    backend_session_id: Option<&str>,
    reused_previous_backend_session: bool,
    previous_binding: Option<crate::daemon_protocol::OpenCodeBinding>,
) -> Option<crate::daemon_protocol::OpenCodeBinding> {
    if !is_http_api {
        None
    } else if reused_previous_backend_session && backend_session_id.is_some() {
        Some(previous_binding.unwrap_or(crate::daemon_protocol::OpenCodeBinding::WeakAdopted))
    } else {
        opencode_binding_for_backend_session(is_http_api, backend_session_id)
    }
}

/// Select the backend identity written by the final restart registration.
///
/// A fresh Codex launch has to read the state again because its SessionStart
/// hook can consume the one-time credential before this refresh. A resumed
/// Codex launch already has an authoritative thread ID: the exact ID passed to
/// `codex resume`, so the refresh must retain it even though TUI backends do
/// not otherwise discover a backend session ID here.
fn final_restart_backend_binding(
    backend_name: &str,
    resume_id: Option<String>,
    session_start_credential: Option<String>,
    discovered_backend_session_id: Option<String>,
    session_start_result: Option<(Option<String>, Option<String>)>,
) -> (Option<String>, Option<String>) {
    if let Some(credential) = session_start_credential {
        return session_start_result.unwrap_or((None, Some(credential)));
    }

    if backend_name == "codex-cli" {
        return (resume_id, None);
    }

    (discovered_backend_session_id, None)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ParentSessionOverride {
    #[default]
    PreservePrevious,
    SetParent(String),
    NoParent,
}

impl ParentSessionOverride {
    pub fn from_request(parent_session: Option<&str>, no_parent_session: bool) -> Self {
        if no_parent_session {
            Self::NoParent
        } else if let Some(parent) = parent_session.map(str::trim).filter(|s| !s.is_empty()) {
            Self::SetParent(parent.to_string())
        } else {
            Self::PreservePrevious
        }
    }

    fn resolve(
        &self,
        previous_metadata: Option<&crate::daemon_protocol::SessionMeta>,
    ) -> Option<String> {
        match self {
            Self::PreservePrevious => previous_metadata.and_then(|m| m.parent_session.clone()),
            Self::SetParent(parent) => Some(parent.clone()),
            Self::NoParent => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RestartPromptInput<'a> {
    replacement: Option<&'a str>,
    suppress_stored: bool,
    one_shot: Option<&'a str>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ResolvedRestartPrompt {
    /// Prompt delivered to this backend launch.
    launch: Option<String>,
    /// Persistent base prompt recorded in session metadata.
    stored: Option<String>,
}

fn resolve_restart_prompt(
    stored: Option<&str>,
    input: RestartPromptInput<'_>,
) -> ResolvedRestartPrompt {
    let persistent = input.replacement.or(stored).map(String::from);
    let base = input.replacement.map(String::from).or_else(|| {
        (!input.suppress_stored)
            .then(|| stored.map(String::from))
            .flatten()
    });
    let launch = match (base, input.one_shot) {
        (Some(base), Some(one_shot)) => Some(format!("{base}\n\n{one_shot}")),
        (Some(base), None) => Some(base),
        (None, Some(one_shot)) => Some(one_shot.to_string()),
        (None, None) => None,
    };

    ResolvedRestartPrompt {
        launch,
        stored: persistent,
    }
}

fn active_context_policy_for_launch(
    previous: Option<u64>,
    requested: Option<u64>,
    fresh: bool,
) -> Option<u64> {
    if fresh {
        requested.or(previous)
    } else {
        previous
    }
}

#[derive(Debug)]
struct PreparedLaunchCommand {
    command: String,
    prompt_path: Option<PathBuf>,
    cleanup_on_drop: bool,
}

impl PreparedLaunchCommand {
    fn command(&self) -> &str {
        &self.command
    }

    #[cfg(test)]
    fn prompt_path(&self) -> Option<&Path> {
        self.prompt_path.as_deref()
    }

    /// Relinquish cleanup after tmux accepts the launch command.
    ///
    /// This transition is deliberately infallible and irreversible: tmux may
    /// already be starting the backend when it returns success. The shell
    /// command reads and unlinks the file before exec, which is the only safe
    /// consumption acknowledgement. A time-based cleanup here could race a
    /// slow login shell and destroy its launch prompt.
    fn mark_handed_off(&mut self) {
        self.prompt_path = None;
        self.cleanup_on_drop = false;
    }
}

impl Drop for PreparedLaunchCommand {
    fn drop(&mut self) {
        if self.cleanup_on_drop
            && let Some(path) = self.prompt_path.as_deref()
        {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        "failed to remove unhanded launch prompt: {error}"
                    );
                }
            }
        }
    }
}

fn prepare_tui_launch_command(
    backend_command: &str,
    prompt: Option<&str>,
) -> anyhow::Result<PreparedLaunchCommand> {
    prepare_tui_launch_command_in(&std::env::temp_dir(), backend_command, prompt)
}

fn prepare_backend_launch_command(
    is_http_api: bool,
    backend_command: &str,
    prompt: Option<&str>,
) -> anyhow::Result<PreparedLaunchCommand> {
    if is_http_api {
        Ok(PreparedLaunchCommand {
            command: backend_command.to_string(),
            prompt_path: None,
            cleanup_on_drop: false,
        })
    } else {
        prepare_tui_launch_command(backend_command, prompt)
    }
}

fn prepare_tui_launch_command_in(
    temp_dir: &Path,
    backend_command: &str,
    prompt: Option<&str>,
) -> anyhow::Result<PreparedLaunchCommand> {
    let Some(prompt) = prompt else {
        return Ok(PreparedLaunchCommand {
            command: backend_command.to_string(),
            prompt_path: None,
            cleanup_on_drop: false,
        });
    };
    prepare_tui_launch_command_in_with_writer(temp_dir, backend_command, prompt, |file, prompt| {
        use std::io::Write;
        file.write_all(prompt.as_bytes())
    })
}

fn prepare_tui_launch_command_in_with_writer(
    temp_dir: &Path,
    backend_command: &str,
    prompt: &str,
    writer: impl FnOnce(&mut std::fs::File, &str) -> std::io::Result<()>,
) -> anyhow::Result<PreparedLaunchCommand> {
    let (path, mut file) = (0..16)
        .find_map(|_| {
            let path = temp_dir.join(format!(
                "ouija-launch-prompt-{}-{:032x}",
                std::process::id(),
                rand::random::<u128>()
            ));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => Some(Ok((path, file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()
        .with_context(|| {
            format!(
                "failed to create private launch prompt in {}",
                temp_dir.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "failed to create unique private launch prompt in {}",
                temp_dir.display()
            )
        })?;

    if let Err(error) = writer(&mut file, prompt) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(error)
            .with_context(|| format!("failed to write private launch prompt {}", path.display()));
    }
    drop(file);

    let Some(path_text) = path.to_str() else {
        let cleanup_error = std::fs::remove_file(&path).err();
        anyhow::bail!(
            "private launch prompt path is not valid UTF-8{}",
            cleanup_error
                .map(|error| format!("; cleanup failed: {error}"))
                .unwrap_or_default()
        );
    };
    let escaped_path = crate::scheduler::shell_escape(path_text);
    let command = format!(
        "ouija_launch_prompt=\"$(cat {escaped_path})\"; \
         ouija_prompt_status=$?; rm -f {escaped_path} || exit $?; \
         [ \"$ouija_prompt_status\" -eq 0 ] || exit \"$ouija_prompt_status\"; \
         {backend_command} \"$ouija_launch_prompt\""
    );

    Ok(PreparedLaunchCommand {
        command,
        prompt_path: Some(path),
        cleanup_on_drop: true,
    })
}

fn start_registration_metadata(
    is_http_api: bool,
    pane_id: &str,
    backend_session_id: Option<String>,
) -> Option<(
    Option<String>,
    Option<String>,
    Option<crate::daemon_protocol::OpenCodeBinding>,
)> {
    if is_http_api && backend_session_id.is_none() {
        return None;
    }

    let opencode_binding =
        opencode_binding_for_backend_session(is_http_api, backend_session_id.as_deref());
    Some((
        Some(pane_id.to_string()),
        backend_session_id,
        opencode_binding,
    ))
}

fn should_schedule_restart_prompt_injection(
    is_http_api: bool,
    backend_session_id: Option<&str>,
    opencode_binding: Option<&crate::daemon_protocol::OpenCodeBinding>,
) -> bool {
    is_http_api
        && backend_session_id.is_some()
        && opencode_binding == Some(&crate::daemon_protocol::OpenCodeBinding::StrongManaged)
}

fn should_cleanup_failed_opencode_attach_pane(
    is_http_api: bool,
    backend_session_id: Option<&str>,
) -> bool {
    is_http_api && backend_session_id.is_none()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IncumbentPaneDisposition {
    Respawn,
    Recreate,
    Refuse,
}

fn classify_incumbent_pane(
    inspection: &crate::tmux::ManagedPaneInspection,
    lease_owner: &crate::daemon_protocol::ResourceOwner,
    restart_target_owner: &crate::daemon_protocol::ResourceOwner,
) -> IncumbentPaneDisposition {
    match inspection {
        crate::tmux::ManagedPaneInspection::Missing => IncumbentPaneDisposition::Recreate,
        crate::tmux::ManagedPaneInspection::ProcessOwner(observed)
        | crate::tmux::ManagedPaneInspection::MarkerOwner(observed)
            if crate::tmux::physical_owner_matches(observed, lease_owner)
                || crate::tmux::physical_owner_matches(observed, restart_target_owner) =>
        {
            IncumbentPaneDisposition::Respawn
        }
        crate::tmux::ManagedPaneInspection::ProcessOwner(_)
        | crate::tmux::ManagedPaneInspection::MarkerOwner(_)
        | crate::tmux::ManagedPaneInspection::Unmanaged => IncumbentPaneDisposition::Refuse,
    }
}

#[cfg(test)]
async fn cleanup_provisional_start(
    state: &std::sync::Arc<AppState>,
    session_id: &str,
    pane_id: &str,
) {
    let owns_provisional_pane = state
        .protocol
        .read()
        .await
        .sessions
        .get(session_id)
        .is_some_and(|s| s.pane.as_deref() == Some(pane_id));
    if !owns_provisional_pane {
        return;
    }

    state
        .apply_and_execute(crate::daemon_protocol::Event::Remove {
            id: session_id.to_string(),
            keep_worktree: true,
        })
        .await;
    if !cfg!(test) {
        let pane = pane_id.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            std::process::Command::new("tmux")
                .args(["kill-pane", "-t", &pane])
                .status()
        })
        .await;
    }
}

async fn cleanup_reserved_start(
    state: &std::sync::Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
    pane_id: &str,
    credential: Option<&str>,
) {
    if let Err(error) = remove_inert_start_pane(state, owner, pane_id).await {
        tracing::warn!(
            session_id = %owner.session_id,
            incarnation = %owner.incarnation,
            "failed to remove exact reserved-start pane; retaining recovery authority: {error}"
        );
        return;
    }
    match state
        .rollback_reserved_start(owner, pane_id, credential)
        .await
    {
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {
            if let Err(error) = state.abort_lifecycle(owner).await {
                tracing::warn!(
                    session_id = %owner.session_id,
                    incarnation = %owner.incarnation,
                    "failed to release rolled-back start reservation: {error}"
                );
            }
        }
        Ok(outcome) => {
            tracing::warn!(
                session_id = %owner.session_id,
                incarnation = %owner.incarnation,
                ?outcome,
                "reserved start rollback no longer owned the launch"
            );
        }
        Err(error) => {
            tracing::warn!(
                session_id = %owner.session_id,
                incarnation = %owner.incarnation,
                "failed to durably roll back reserved start: {error}"
            );
        }
    }
}

/// Timeout when waiting for relay connections to establish.
const RELAY_CONNECT_TIMEOUT_SECS: u64 = 5;
/// Maximum size of the seen-events dedup cache before clearing.
const SEEN_EVENTS_CACHE_LIMIT: usize = 2048;
/// Timeout for the claude process to exit after sending /exit.
const PROCESS_EXIT_TIMEOUT_SECS: u64 = 10;
/// Length threshold for truncating npub display strings.
const NPUB_TRUNCATE_LEN: usize = 20;

/// Nostr-based transport using NIP-17 private direct messages.
///
/// Each daemon is a Nostr identity. Messages are sent as gift-wrapped
/// DMs (NIP-59) through standard Nostr relays.
pub struct NostrTransport {
    client: Client,
    keys: Keys,
    relay_urls: RwLock<Vec<String>>,
    peer_pubkeys: RwLock<HashSet<PublicKey>>,
    connect_secret: RwLock<String>,
    data_dir: PathBuf,
    ready: AtomicBool,
}

impl std::fmt::Debug for NostrTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NostrTransport")
            .field("data_dir", &self.data_dir)
            .field("ready", &self.ready.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl NostrTransport {
    /// Create a new Nostr transport and connect to relays.
    pub async fn new(
        keys: Keys,
        relay_urls: Vec<String>,
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
                .wait_for_connection(std::time::Duration::from_secs(RELAY_CONNECT_TIMEOUT_SECS))
                .await;
        }

        let ready = !relay_urls.is_empty();

        let peer_pubkeys = load_peer_pubkeys(&data_dir);

        // Clean up legacy connect_secret file from disk
        match std::fs::remove_file(data_dir.join("connect_secret")) {
            Ok(()) => tracing::info!("removed legacy connect_secret file from disk"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("failed to remove legacy connect_secret file: {e}"),
        }

        Ok(Self {
            client,
            keys,
            relay_urls: RwLock::new(relay_urls),
            peer_pubkeys: RwLock::new(peer_pubkeys),
            connect_secret: RwLock::new(generate_secret()),
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
                                let mut seen = seen_events.lock().expect("seen_events mutex poisoned");
                                if !seen.insert(event.id) {
                                    tracing::debug!(
                                        "skipping duplicate gift-wrap event {}",
                                        event.id
                                    );
                                    return Ok(false);
                                }
                                // Prevent unbounded growth — duplicates only
                                // arrive within seconds, so purging is safe.
                                if seen.len() > SEEN_EVENTS_CACHE_LIMIT {
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
                                                    let current_secret = transport.connect_secret.read().await.clone();
                                                    if secret == current_secret {
                                                        transport.authorize_peer(sender).await;
                                                        // Void the secret — each ticket is single-use
                                                        *transport.connect_secret.write().await = generate_secret();
                                                        tracing::info!(
                                                        "peer authorized via connect secret: {npub}"
                                                    );
                                                        if !relays.is_empty() {
                                                            transport
                                                                .merge_relays(&relays)
                                                                .await;
                                                        }

                                                        // Persist connection so we can reconnect after restart
                                                        {
                                                            let peer_relay_urls: Vec<RelayUrl> = relays
                                                                .iter()
                                                                .filter_map(|u| RelayUrl::parse(u).ok())
                                                                .collect();
                                                            let relay_urls = if peer_relay_urls.is_empty() {
                                                                let urls = transport.relay_urls.read().await;
                                                                urls.iter()
                                                                    .filter_map(|u| RelayUrl::parse(u).ok())
                                                                    .collect()
                                                            } else {
                                                                peer_relay_urls
                                                            };
                                                            let profile = Nip19Profile::new(sender, relay_urls);
                                                            if let Ok(nprofile) = profile.to_bech32() {
                                                                if let Err(e) = crate::persistence::add_connection(
                                                                    &state.config.data_dir,
                                                                    &nprofile,
                                                                    None,
                                                                    Some(&npub),
                                                                ) {
                                                                    tracing::warn!("failed to persist inbound connection: {e}");
                                                                }
                                                            }
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
                                                        Some(&npub),
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
                .wait_for_connection(std::time::Duration::from_secs(RELAY_CONNECT_TIMEOUT_SECS))
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

        let secret = self.connect_secret.read().await;
        let profile = Nip19Profile::new(self.keys.public_key(), relay_urls);
        profile
            .to_bech32()
            .ok()
            .map(|bech32| format!("{bech32}#{secret}"))
    }

    async fn regenerate(&self, config_dir: &Path, data_dir: &Path) -> anyhow::Result<String> {
        // For nostr, regenerating means generating new keys + new secret
        let new_keys = Keys::generate();

        // Persist the new nsec to config dir
        save_nsec(config_dir, &new_keys)?;

        // Generate new in-memory connect secret
        let new_secret = generate_secret();
        *self.connect_secret.write().await = new_secret.clone();

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
        HumanCommand::Command(cmd) => {
            let reply = handle_human_command(state, &cmd).await;
            if let Err(e) = send_plain_dm(state, npub, &reply).await {
                tracing::warn!("failed to send command reply to {human_name}: {e}");
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
                match crate::router::classify(config, &message, &sessions, &messages, human_name)
                    .await
                {
                    Ok(Some(crate::router::RouterDecision::Route { targets })) => {
                        let valid_targets: Vec<String> = {
                            let proto = state.protocol.read().await;
                            targets
                                .into_iter()
                                .filter(|t| proto.sessions.contains_key(t))
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
    Command(String),
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
    // Session/node management commands
    if text.starts_with("/connect ")
        || text.starts_with("/disconnect ")
        || text.starts_with("/nodes")
        || text.starts_with("/task ")
        || text.starts_with("/kill ")
        || text.starts_with("/start ")
        || text.starts_with("/restart ")
    {
        return HumanCommand::Command(text.to_string());
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
    lines.push(String::new());
    lines.push("Management:".to_string());
    lines.push("  /kill <session>    — kill a session".to_string());
    lines.push("  /start <name>      — start new session".to_string());
    lines.push(
        "  /restart <name> [--fresh]  — restart a session (--fresh: no prior context)".to_string(),
    );
    lines.push("  /connect <ticket>  — connect to peer".to_string());
    lines.push("  /nodes             — list connected nodes".to_string());
    lines.push("  /task list|trigger — manage tasks".to_string());

    lines.join("\n")
}

async fn format_session_list(state: &AppState, human_name: &str) -> String {
    let proto = state.protocol.read().await;
    let default = state
        .settings
        .read()
        .await
        .human_sessions
        .iter()
        .find(|h| h.name == human_name)
        .and_then(|h| h.default_session.clone());

    let mut lines = Vec::new();
    for s in proto.sessions.values() {
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
    let exists = state
        .protocol
        .read()
        .await
        .sessions
        .contains_key(session_id);
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
    let proto = state.protocol.read().await;
    let nodes = state.nodes.read().await;
    let transports = state.transports().await;

    let local = proto
        .sessions
        .values()
        .filter(|s| matches!(s.origin, crate::daemon_protocol::Origin::Local))
        .count();
    let remote = proto
        .sessions
        .values()
        .filter(|s| matches!(s.origin, crate::daemon_protocol::Origin::Remote(_)))
        .count();
    let human = proto
        .sessions
        .values()
        .filter(|s| matches!(s.origin, crate::daemon_protocol::Origin::Human(_)))
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

async fn route_human_message(
    state: &std::sync::Arc<AppState>,
    from: &str,
    to: &str,
    message: &str,
) {
    // Use the same send path as the API
    let target = state.protocol.read().await.sessions.get(to).cloned();

    match target {
        Some(session) => match &session.origin {
            crate::daemon_protocol::Origin::Local => {
                if let Some(pane) = &session.pane {
                    // Human messages always expect a reply
                    let msg_id = {
                        let mut proto = state.protocol.write().await;
                        proto.next_seq()
                    };
                    let formatted = crate::daemon_protocol::format_session_message(
                        from, message, true, msg_id, None, false,
                    );
                    let delivery_method = if session.metadata.backend.as_deref() == Some("opencode")
                    {
                        Some("http")
                    } else {
                        Some("tmux")
                    };
                    let outcome = crate::state::deliver_inject_message_effect(
                        state,
                        crate::state::InjectDeliveryRequest {
                            session_id: to,
                            pane,
                            message: &formatted,
                            vim_mode: session.metadata.vim_mode,
                            delivery_method,
                            recorded_method: None,
                        },
                    )
                    .await;
                    let delivered = matches!(outcome, crate::state::DeliveryOutcome::Accepted);
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
            crate::daemon_protocol::Origin::Remote(_) => {
                let wire_to = crate::daemon_protocol::strip_remote_prefix(to).to_string();
                let msg_id = {
                    let mut proto = state.protocol.write().await;
                    proto.next_seq()
                };
                let wire_msg = crate::protocol::WireMessage::SessionSend {
                    from: from.to_string(),
                    to: wire_to,
                    message: message.to_string(),
                    expects_reply: true,
                    msg_id,
                    responds_to: None,
                    done: false,
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
            crate::daemon_protocol::Origin::Human(npub) => {
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

/// Dispatch a human DM command (e.g. /connect, /kill, /start).
pub async fn handle_human_command(state: &std::sync::Arc<AppState>, cmd: &str) -> String {
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
            if s.len() > NPUB_TRUNCATE_LEN {
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
        let rest = cmd
            .strip_prefix("/task ")
            .expect("prefix checked by starts_with")
            .trim();
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
        kill_session(state, name).await.message
    } else if let Some(rest) = cmd.strip_prefix("/start ") {
        let name = rest.trim();
        // /start chat-command never resets — no base_branch supplied anyway.
        start_session(
            state, name, None, None, None, None, None, None, None, None, None, None, None, None,
            None, false, None,
        )
        .await
        .0
    } else if let Some(rest) = cmd.strip_prefix("/restart ") {
        let rest = rest.trim();
        let (name, fresh) = if let Some(name) = rest.strip_suffix(" --fresh") {
            (name.trim(), true)
        } else if let Some(name) = rest.strip_prefix("--fresh ") {
            (name.trim(), true)
        } else {
            (rest, false)
        };
        restart_session(
            state,
            name,
            fresh,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
        )
        .await
        .0
    } else {
        "unknown command".to_string()
    }
}

/// Machine-readable terminal state for an explicit session kill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KillOutcome {
    Removed,
    Failed,
    Superseded,
}

/// Typed kill result paired with the existing human-facing status text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KillSessionResult {
    pub message: String,
    pub outcome: KillOutcome,
}

impl KillSessionResult {
    fn removed(message: String) -> Self {
        Self {
            message,
            outcome: KillOutcome::Removed,
        }
    }

    fn failed(message: String) -> Self {
        Self {
            message,
            outcome: KillOutcome::Failed,
        }
    }

    fn superseded(message: String) -> Self {
        Self {
            message,
            outcome: KillOutcome::Superseded,
        }
    }
}

/// Kill the Claude process in a named session's pane.
pub async fn kill_session(state: &std::sync::Arc<AppState>, name: &str) -> KillSessionResult {
    kill_session_inner(state, name, false, None).await
}

pub async fn kill_session_keep_worktree(
    state: &std::sync::Arc<AppState>,
    name: &str,
) -> KillSessionResult {
    kill_session_inner(state, name, true, None).await
}

/// Kill only the exact idle-session snapshot selected by max-session eviction.
pub async fn kill_session_owned(
    state: &std::sync::Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
    expected_pane: &str,
) -> KillSessionResult {
    kill_session_inner(
        state,
        &owner.session_id,
        false,
        Some((owner.clone(), expected_pane.to_string())),
    )
    .await
}

async fn kill_session_inner(
    state: &std::sync::Arc<AppState>,
    name: &str,
    keep_worktree: bool,
    expected_owner: Option<(crate::daemon_protocol::ResourceOwner, String)>,
) -> KillSessionResult {
    let session = state.protocol.read().await.sessions.get(name).cloned();
    let Some(session) = session else {
        return KillSessionResult::failed(format!("session '{name}' not found"));
    };
    if !matches!(session.origin, crate::daemon_protocol::Origin::Local) {
        return KillSessionResult::failed(format!("'{name}' is not a local session"));
    }
    let Some(pane) = &session.pane else {
        return KillSessionResult::failed(format!("'{name}' has no pane"));
    };
    if expected_owner
        .as_ref()
        .is_some_and(|(owner, expected_pane)| session.owner() != *owner || pane != expected_pane)
    {
        return KillSessionResult::superseded(format!(
            "session '{name}' was replaced before eviction"
        ));
    }
    let pane = pane.clone();
    let owner = session.owner();
    match state
        .claim_existing_stop(&owner, &pane, !keep_worktree)
        .await
    {
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
        Ok(outcome) => {
            return KillSessionResult::superseded(format!(
                "session '{name}' backend exit was superseded ({outcome:?})"
            ));
        }
        Err(error) => {
            return KillSessionResult::failed(format!(
                "failed to persist backend exit authority for '{name}': {error}"
            ));
        }
    }
    let claimed_session = {
        let protocol = state.protocol.read().await;
        let lease_matches = protocol.lifecycle_leases.get(name).is_some_and(|lease| {
            lease.owner == owner && lease.phase == crate::daemon_protocol::LifecyclePhase::Stopping
        });
        protocol
            .sessions
            .get(name)
            .filter(|current| {
                lease_matches
                    && current.owner() == owner
                    && current.pane.as_deref() == Some(pane.as_str())
            })
            .cloned()
    };
    let Some(claimed_session) = claimed_session else {
        let _ = state.abort_lifecycle(&owner).await;
        return KillSessionResult::superseded(format!(
            "session '{name}' backend exit was superseded after claim"
        ));
    };
    let project_dir = claimed_session.metadata.project_dir.clone();
    let backend_session_id = claimed_session.metadata.backend_session_id.clone();
    let backend = claimed_session
        .metadata
        .backend
        .as_deref()
        .and_then(|backend| state.backends.get(backend))
        .unwrap_or_else(|| state.backends.default());
    let is_http_api = matches!(
        backend.delivery_mode(),
        crate::backend::DeliveryMode::HttpApi { .. }
    );
    if is_http_api && backend_session_id.is_none() {
        let release = state.abort_lifecycle(&owner).await;
        return match release {
            Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {
                KillSessionResult::failed(format!(
                    "HttpApi session '{name}' has no backend session ID; no external cleanup was started"
                ))
            }
            Ok(outcome) => KillSessionResult::superseded(format!(
                "HttpApi session '{name}' has no backend session ID and its stop claim was superseded ({outcome:?})"
            )),
            Err(error) => KillSessionResult::failed(format!(
                "HttpApi session '{name}' has no backend session ID and its unused stop claim could not be released: {error}"
            )),
        };
    }
    let process_names: Vec<String> = backend
        .process_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let exit_cmd = backend.exit_command().map(String::from);
    let cli_name = backend.cli_name().to_string();

    // For HttpApi backends (opencode), abort the server-side session BEFORE
    // killing the client process. The attach client is just a TUI — killing
    // it does NOT stop the server from executing the current assistant turn.
    if is_http_api {
        if !state.owns_stopping_session(&owner, &pane).await {
            let _ = state.abort_lifecycle(&owner).await;
            return KillSessionResult::superseded(format!(
                "session '{name}' backend exit was superseded before abort"
            ));
        }
        if let Some(ref oc_sid) = backend_session_id {
            let port = state.opencode_serve_port();
            let url = format!("http://127.0.0.1:{port}/session/{oc_sid}/abort");
            let response = state
                .with_owned_backend_cleanup(&owner, oc_sid, || async {
                    state
                        .http_client
                        .post(&url)
                        .timeout(std::time::Duration::from_secs(5))
                        .send()
                        .await
                })
                .await;
            let Some(response) = response else {
                let _ = state.abort_lifecycle(&owner).await;
                return KillSessionResult::superseded(format!(
                    "session '{name}' backend exit was superseded before abort"
                ));
            };
            match response {
                Ok(r)
                    if r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND =>
                {
                    tracing::info!(session = %name, oc_sid, "aborted opencode server session");
                }
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();
                    return KillSessionResult::failed(format!(
                        "opencode abort for session '{name}' returned {status}: {text}; stop authority retained for recovery"
                    ));
                }
                Err(e) => {
                    return KillSessionResult::failed(format!(
                        "opencode abort for session '{name}' failed: {e}; stop authority retained for recovery"
                    ));
                }
            }
        }
    }

    // Keep the registry row and durable stop lease until the external exit is
    // complete. SessionEnd is owner-scoped and the lease prevents it (or a
    // same-ID registration) from releasing this identity while we await.
    let pane_owner = owner.clone();
    let pane_for_kill = pane.clone();
    let state_for_kill = std::sync::Arc::clone(state);
    let runtime = tokio::runtime::Handle::current();
    let gate_owner = owner.clone();
    let gate_pane = pane.clone();
    let kill_result = state
        .with_owned_pane_cleanup(&gate_owner, &gate_pane, move || async move {
            tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
                use std::process::Command;

                let pane_still_owned = || -> anyhow::Result<bool> {
                    if !runtime
                        .block_on(state_for_kill.owns_stopping_session(&pane_owner, &pane_for_kill))
                    {
                        anyhow::bail!("stop authority changed before backend exit");
                    }
                    match crate::tmux::inspect_managed_pane(&pane_for_kill)? {
                        crate::tmux::ManagedPaneInspection::Missing => Ok(false),
                        crate::tmux::ManagedPaneInspection::ProcessOwner(observed)
                        | crate::tmux::ManagedPaneInspection::MarkerOwner(observed)
                            if crate::tmux::physical_owner_matches(&observed, &pane_owner) =>
                        {
                            Ok(true)
                        }
                        crate::tmux::ManagedPaneInspection::ProcessOwner(_)
                        | crate::tmux::ManagedPaneInspection::MarkerOwner(_)
                        | crate::tmux::ManagedPaneInspection::Unmanaged => {
                            anyhow::bail!("pane owner changed before backend exit");
                        }
                    }
                };
                let process_alive = |pid: u32| -> anyhow::Result<bool> {
                    Ok(Command::new("kill")
                        .args(["-0", &pid.to_string()])
                        .status()?
                        .success())
                };
                let kill_owned_pane = || -> anyhow::Result<()> {
                    if !pane_still_owned()? {
                        return Ok(());
                    }
                    let status = Command::new("tmux")
                        .args(["kill-pane", "-t", &pane_for_kill])
                        .status()?;
                    match crate::tmux::inspect_managed_pane(&pane_for_kill)? {
                        crate::tmux::ManagedPaneInspection::Missing => {}
                        crate::tmux::ManagedPaneInspection::ProcessOwner(observed)
                        | crate::tmux::ManagedPaneInspection::MarkerOwner(observed)
                            if crate::tmux::physical_owner_matches(
                                &observed,
                                &pane_owner,
                            ) =>
                        {
                            anyhow::bail!(
                                "tmux kill-pane left the owned pane alive (status {status})"
                            );
                        }
                        crate::tmux::ManagedPaneInspection::ProcessOwner(_)
                        | crate::tmux::ManagedPaneInspection::MarkerOwner(_)
                        | crate::tmux::ManagedPaneInspection::Unmanaged => {
                            anyhow::bail!("pane owner changed during backend exit");
                        }
                    }
                    Ok(())
                };

                // The HTTP abort can make an attach client exit before local
                // pane cleanup begins. A truly missing pane is already clean;
                // a live unmanaged or differently-owned pane still fails closed.
                if !pane_still_owned()? {
                    return Ok("pane already exited".to_string());
                }

                // Get pane PID
                let output = Command::new("tmux")
                    .args(["display-message", "-t", &pane_for_kill, "-p", "#{pane_pid}"])
                    .output()?;
                if !output.status.success() {
                    if matches!(
                        crate::tmux::inspect_managed_pane(&pane_for_kill)?,
                        crate::tmux::ManagedPaneInspection::Missing
                    ) {
                        return Ok("pane already exited".to_string());
                    }
                    anyhow::bail!("could not get pane PID");
                }
                let pid_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let pane_pid: u32 = match pid_str.parse() {
                    Ok(pid) => pid,
                    Err(_) => {
                        // Pane exists but has no running process — skip process kill, just clean up
                        kill_owned_pane()?;
                        return Ok("no running process in pane".to_string());
                    }
                };

                // Find backend process in the tree
                let output = Command::new("ps").args(["-eo", "pid,ppid,comm"]).output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let mut children: std::collections::HashMap<u32, Vec<u32>> =
                    std::collections::HashMap::new();
                let mut names: std::collections::HashMap<u32, String> =
                    std::collections::HashMap::new();

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

                // BFS to find backend PID.
                // Match both exact name and dot-prefixed name (e.g. ".opencode"
                // which appears when run via npm/node wrapper).
                let mut stack = vec![pane_pid];
                let mut backend_pid = None;
                while let Some(pid) = stack.pop() {
                    if names.get(&pid).is_some_and(|n| {
                        process_names
                            .iter()
                            .any(|pn| pn == n || n.strip_prefix('.') == Some(pn.as_str()))
                    }) {
                        backend_pid = Some(pid);
                        break;
                    }
                    if let Some(kids) = children.get(&pid) {
                        stack.extend(kids);
                    }
                }

                match backend_pid {
                    Some(pid) => {
                        let mut exited = false;
                        // When preserving worktrees, skip graceful /exit — the
                        // backend may clean up its own worktree during exit.
                        // Go straight to SIGKILL to prevent cleanup handlers.
                        if keep_worktree {
                            if !pane_still_owned()? {
                                return Ok("pane already exited".to_string());
                            }
                            let signal_status =
                                Command::new("kill").args(["-9", &pid.to_string()]).status()?;
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            if process_alive(pid)? {
                                anyhow::bail!(
                                    "SIGKILL did not stop {cli_name} pid {pid} (status {signal_status})"
                                );
                            }
                        } else {
                            // Graceful: send exit command if backend supports it
                            if let Some(ref exit) = exit_cmd {
                                if !pane_still_owned()? {
                                    return Ok("pane already exited".to_string());
                                }
                                let _send_status = Command::new("tmux")
                                    .args(["send-keys", "-t", &pane_for_kill, exit, "Enter"])
                                    .status()?;

                                // Poll up to 10s for process to exit
                                let deadline = std::time::Instant::now()
                                    + std::time::Duration::from_secs(PROCESS_EXIT_TIMEOUT_SECS);
                                while std::time::Instant::now() < deadline {
                                    std::thread::sleep(std::time::Duration::from_secs(1));
                                    if !process_alive(pid)? {
                                        exited = true;
                                        break;
                                    }
                                }
                            }

                            if !exited {
                                // Fallback: SIGTERM
                                if !pane_still_owned()? {
                                    return Ok("pane already exited".to_string());
                                }
                                let _signal_status =
                                    Command::new("kill").arg(pid.to_string()).status()?;
                                std::thread::sleep(std::time::Duration::from_secs(1));
                                exited = !process_alive(pid)?;
                            }
                        }

                        kill_owned_pane()?;
                        if !exited && process_alive(pid)? {
                            let signal_status =
                                Command::new("kill").args(["-9", &pid.to_string()]).status()?;
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            if process_alive(pid)? {
                                anyhow::bail!(
                                    "pane cleanup left {cli_name} pid {pid} alive (SIGKILL status {signal_status})"
                                );
                            }
                        }
                        let method = if keep_worktree {
                            "SIGKILL (worktree preserved)"
                        } else if exited {
                            "exited gracefully"
                        } else {
                            "SIGTERM"
                        };
                        Ok(format!("killed {cli_name} (pid {pid}, {method})"))
                    }
                    None => {
                        kill_owned_pane()?;
                        Ok(format!("no {cli_name} process found"))
                    }
                }
            })
            .await
        })
        .await
        .unwrap_or_else(|| {
            Ok(Err(anyhow::anyhow!(
                "pane owner changed before backend exit"
            )))
        });

    if matches!(
        &kill_result,
        Ok(Err(error))
            if error.to_string().contains("pane owner changed")
                || error.to_string().contains("stop authority changed")
    ) {
        let _ = state.abort_lifecycle(&owner).await;
        return KillSessionResult::superseded(format!(
            "session '{name}' pane was replaced before backend exit: {}",
            match &kill_result {
                Ok(Err(error)) => error,
                _ => unreachable!("matched kill result must contain an inner error"),
            }
        ));
    }

    let msg = match kill_result {
        Ok(Ok(msg)) => msg,
        Ok(Err(error)) => {
            return KillSessionResult::failed(format!(
                "session '{name}' backend exit failed: {error}; stop authority retained for recovery"
            ));
        }
        Err(error) => {
            return KillSessionResult::failed(format!(
                "session '{name}' backend exit task failed: {error}; stop authority retained for recovery"
            ));
        }
    };

    let removal_effects = state
        .apply_and_execute(crate::daemon_protocol::Event::CompleteOwnedStop {
            owner: owner.clone(),
            expected_pane: pane.clone(),
            keep_worktree: true,
        })
        .await;
    if !removal_effects.iter().any(|effect| {
        matches!(
            effect,
            crate::daemon_protocol::Effect::RemoveOk { id } if id == name
        )
    }) {
        let _ = state.abort_lifecycle(&owner).await;
        return KillSessionResult::superseded(format!(
            "session '{name}' backend exit completion was superseded"
        ));
    }

    // Worktree cleanup AFTER the process is confirmed dead, so we don't
    // race against claude writing to its cwd. Mirrors the shared-worktree
    // guard in apply_remove: skip cleanup if another session still uses
    // the same directory.
    if !keep_worktree {
        if let Some(dir) = project_dir {
            let is_worktree_path =
                dir.contains("/.ouija/worktrees/") || dir.contains("/.claude/worktrees/");
            if is_worktree_path {
                state.cleanup_worktree_dir_if_unused(&owner, &dir).await;
            }
        }
    }

    match state.abort_lifecycle(&owner).await {
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {
            KillSessionResult::removed(format!("{msg}, session '{name}' removed"))
        }
        Ok(outcome) => KillSessionResult::superseded(format!(
            "session '{name}' cleanup completion was superseded ({outcome:?})"
        )),
        Err(error) => KillSessionResult::failed(format!(
            "session '{name}' was removed but stop authority could not be released: {error}"
        )),
    }
}

/// Start a new session in a tmux pane, optionally in a worktree.
pub(crate) async fn reserve_start_for_launch(
    state: &std::sync::Arc<AppState>,
    name: &str,
) -> anyhow::Result<crate::daemon_protocol::StartDisposition> {
    state.reserve_start(name).await
}

async fn fail_reserved_start(
    state: &std::sync::Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
    message: String,
) -> (String, Option<u64>) {
    match state.abort_lifecycle(owner).await {
        Ok(_) => (message, None),
        Err(error) => (
            format!("{message}; failed to release lifecycle reservation: {error}"),
            None,
        ),
    }
}

async fn remove_inert_start_pane(
    state: &std::sync::Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
    pane: &str,
) -> anyhow::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    let owner = owner.clone();
    let pane = pane.to_string();
    let pane_for_guard = pane.clone();
    let owner_for_guard = owner.clone();
    state
        .with_owned_pane_cleanup(&owner_for_guard, &pane_for_guard, move || async move {
            tokio::task::spawn_blocking(move || {
                if !crate::tmux::inspect_pane_owner(&pane)?
                    .as_ref()
                    .is_some_and(|observed| crate::tmux::physical_owner_matches(observed, &owner))
                {
                    return Ok(());
                }
                let status = std::process::Command::new("tmux")
                    .args(["kill-pane", "-t", &pane])
                    .status()
                    .with_context(|| format!("failed to kill inert start pane {pane}"))?;
                if !status.success()
                    && crate::tmux::inspect_pane_owner(&pane)?
                        .as_ref()
                        .is_some_and(|observed| {
                            crate::tmux::physical_owner_matches(observed, &owner)
                        })
                {
                    anyhow::bail!(
                        "failed to remove inert pane {pane} for {} incarnation {}",
                        owner.session_id,
                        owner.incarnation
                    );
                }
                Ok(())
            })
            .await
            .context("inert start pane cleanup task failed")?
        })
        .await
        .unwrap_or(Ok(()))
}

async fn finalize_reserved_start(
    state: &std::sync::Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
    pane: Option<String>,
    metadata: crate::daemon_protocol::SessionMeta,
) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
    state.finalize_reserved_start(owner, pane, metadata).await
}

/// Start a new session in a tmux pane, optionally in a worktree.
#[allow(clippy::too_many_arguments)]
pub async fn start_session(
    state: &std::sync::Arc<AppState>,
    name: &str,
    worktree: Option<bool>,
    project_dir: Option<&str>,
    prompt: Option<&str>,
    from: Option<&str>,
    expects_reply: Option<bool>,
    backend: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    reminder: Option<&str>,
    parent_session: Option<&str>,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
    branch: Option<&str>,
    base_branch: Option<&str>,
    force_reset: bool,
    reserved_owner: Option<crate::daemon_protocol::ResourceOwner>,
) -> (String, Option<u64>) {
    start_session_with_active_context_policy(
        state,
        name,
        worktree,
        project_dir,
        prompt,
        from,
        expects_reply,
        backend,
        model,
        effort,
        reminder,
        parent_session,
        idle_policy,
        branch,
        base_branch,
        force_reset,
        None,
        reserved_owner,
    )
    .await
}

/// Start a new session with an API-validated active-context policy.
#[allow(clippy::too_many_arguments)]
pub async fn start_session_with_active_context_policy(
    state: &std::sync::Arc<AppState>,
    name: &str,
    worktree: Option<bool>,
    project_dir: Option<&str>,
    prompt: Option<&str>,
    from: Option<&str>,
    expects_reply: Option<bool>,
    backend: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    reminder: Option<&str>,
    parent_session: Option<&str>,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
    branch: Option<&str>,
    base_branch: Option<&str>,
    force_reset: bool,
    fresh_context_after_active_secs: Option<u64>,
    reserved_owner: Option<crate::daemon_protocol::ResourceOwner>,
) -> (String, Option<u64>) {
    start_session_with_prompt_storage(
        state,
        name,
        worktree,
        project_dir,
        prompt,
        prompt,
        from,
        expects_reply,
        backend,
        model,
        effort,
        reminder,
        parent_session,
        idle_policy,
        branch,
        base_branch,
        force_reset,
        fresh_context_after_active_secs,
        reserved_owner,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_session_with_prompt_storage(
    state: &std::sync::Arc<AppState>,
    name: &str,
    worktree: Option<bool>,
    project_dir: Option<&str>,
    prompt: Option<&str>,
    stored_prompt: Option<&str>,
    from: Option<&str>,
    expects_reply: Option<bool>,
    backend: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    reminder: Option<&str>,
    parent_session: Option<&str>,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
    branch: Option<&str>,
    base_branch: Option<&str>,
    force_reset: bool,
    fresh_context_after_active_secs: Option<u64>,
    reserved_owner: Option<crate::daemon_protocol::ResourceOwner>,
) -> (String, Option<u64>) {
    let backend = match backend {
        Some(b) => match state.backends.get_required(b) {
            Ok(backend) => backend,
            Err(message) => return (message, None),
        },
        None => state.backends.default(),
    };
    let backend_name = backend.name().to_string();

    let owner = match reserved_owner {
        Some(owner) => {
            let owns_lease = state
                .protocol
                .read()
                .await
                .lifecycle_leases
                .get(name)
                .is_some_and(|lease| lease.owner == owner);
            if !owns_lease || owner.session_id != name {
                return (
                    format!("start for session '{name}' was superseded before launch"),
                    None,
                );
            }
            owner
        }
        None => match reserve_start_for_launch(state, name).await {
            Ok(crate::daemon_protocol::StartDisposition::Reserved(owner)) => owner,
            Ok(crate::daemon_protocol::StartDisposition::Existing(_)) => {
                return (format!("session '{name}' already exists"), None);
            }
            Ok(crate::daemon_protocol::StartDisposition::InProgress(_)) => {
                return (format!("session '{name}' start already in progress"), None);
            }
            Err(error) => {
                return (
                    format!("failed to reserve session '{name}' for launch: {error}"),
                    None,
                );
            }
        },
    };

    let mut dir = if let Some(pd) = project_dir {
        pd.to_string()
    } else {
        let projects_dir = state.settings.read().await.projects_dir.clone();
        let base = match projects_dir {
            Some(dir) => crate::state::expand_tilde(&dir),
            None => crate::state::expand_tilde("~/code"),
        };
        format!("{base}/{name}")
    };

    // Auto-enable worktree if another session shares this directory AND it's a git repo
    let is_git_repo = std::path::Path::new(&dir).join(".git").exists();
    let (worktree, auto_worktree) = match worktree {
        Some(wt) if wt && !is_git_repo => {
            tracing::warn!("worktree requested but {dir} is not a git repo, disabling");
            (false, false)
        }
        Some(wt) => (wt, false),
        None => {
            let proto = state.protocol.read().await;
            let conflict = proto.sessions.values().any(|s| {
                matches!(s.origin, crate::daemon_protocol::Origin::Local)
                    && s.metadata.project_dir.as_deref() == Some(dir.as_str())
            });
            if conflict && !is_git_repo {
                tracing::warn!(
                    "directory conflict for {dir} but not a git repo, skipping auto-worktree"
                );
            }
            let auto = conflict && is_git_repo;
            (auto, auto)
        }
    };

    // If worktree requested, ouija creates it in .ouija/worktrees/<name>.
    // The backend never sees --worktree — it just gets a directory.
    if worktree {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let candidates = ouija_worktree_candidates(&dir, name, std::path::Path::new(&home));
        let legacy_candidate = candidates[0].clone();
        let new_candidate = candidates[1].clone();
        let repo_dir = dir.clone();
        // `force_reset` is the explicit caller opt-in that guards against
        // silent branch wipes on respawn (hub#528). Threaded from the API
        // boundary; defaults to false.
        let claim = state
            .with_reserved_project_dir_choice(
                &owner,
                owner.clone(),
                &candidates,
                move || {
                    if std::path::Path::new(&legacy_candidate).exists() {
                        legacy_candidate
                    } else {
                        new_candidate
                    }
                },
                move |worktree_dir| async move {
                    create_ouija_worktree_at(
                        &repo_dir,
                        name,
                        branch,
                        base_branch,
                        force_reset,
                        &worktree_dir,
                    )
                },
            )
            .await;
        match claim {
            Ok(Some(Ok(wt_dir))) => dir = wt_dir,
            Ok(Some(Err(error))) => {
                return fail_reserved_start(
                    state,
                    &owner,
                    format!("failed to create worktree: {error}"),
                )
                .await;
            }
            Ok(None) => {
                return (
                    format!("start for session '{name}' was superseded before worktree creation"),
                    None,
                );
            }
            Err(error) => {
                return fail_reserved_start(
                    state,
                    &owner,
                    format!("failed to persist worktree claim: {error}"),
                )
                .await;
            }
        }
    } else {
        let claim = state
            .with_reserved_project_dir_claim(&owner, owner.clone(), &dir, false, || async {
                std::fs::create_dir_all(&dir)
            })
            .await;
        match claim {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(error))) => {
                return fail_reserved_start(
                    state,
                    &owner,
                    format!("failed to create {dir}: {error}"),
                )
                .await;
            }
            Ok(None) => {
                return (
                    format!("start for session '{name}' was superseded before directory creation"),
                    None,
                );
            }
            Err(error) => {
                return fail_reserved_start(
                    state,
                    &owner,
                    format!("failed to persist directory claim: {error}"),
                )
                .await;
            }
        }
    }

    let tmux_session = crate::tmux::tmux_session_name(&dir);
    let window_name = name.to_string();
    let settings = state.settings.read().await;
    let claude_permission_mode = settings.claude_permission_mode.clone();
    let launch_model = crate::backend::resolve_launch_model_config(
        &backend_name,
        model.map(String::from),
        &settings,
    );
    drop(settings);
    crate::backend::codex::install_configured_home(launch_model.codex_home.as_deref());
    // Mint the proof before rendering the command: a shared Codex app-server
    // cannot rely on pane-local env, so fresh launches receive it as a trusted
    // session-flags hook instead.
    let session_start_credential =
        (backend_name == "codex-cli").then(crate::daemon_protocol::new_session_start_credential);
    let backend_cmd = backend.build_start_command(&crate::backend::StartOpts {
        project_dir: dir.clone(),
        worktree: None, // ouija manages worktrees, not the backend
        model: launch_model.model.clone(),
        effort: effort.map(String::from),
        permission_mode: claude_permission_mode,
        codex_home: launch_model.codex_home.clone(),
    });
    let backend_cmd = match session_start_credential.as_deref() {
        Some(credential) => match crate::backend::codex::with_session_start_hook(
            backend_cmd,
            launch_model.codex_home.as_deref(),
            name,
            credential,
            owner.incarnation,
        ) {
            Ok(command) => command,
            Err(error) => {
                return fail_reserved_start(
                    state,
                    &owner,
                    format!("could not stage Codex launch credential: {error}"),
                )
                .await;
            }
        },
        None => backend_cmd,
    };

    let reminder_meta = crate::daemon_protocol::SessionMeta {
        reminder: reminder.map(String::from),
        parent_session: parent_session.map(String::from),
        idle_policy: idle_policy.clone(),
        ..Default::default()
    };
    let effective_reminder = reminder_meta.effective_reminder(name, None);

    // Pre-compute the prompt text and sender envelope before launching, so we
    // can write it to a temp file for CLI arg delivery.
    let pre_queued_prompt = if let Some(text) = prompt {
        let full_text = match effective_reminder.as_deref() {
            Some(r) => format!("{text}\n\n{r}"),
            None => text.to_string(),
        };
        if let Some(sender) = from {
            let er = expects_reply.unwrap_or(true);
            let msg_id = {
                let mut proto = state.protocol.write().await;
                proto.next_seq()
            };
            let formatted = crate::daemon_protocol::format_session_message(
                sender, &full_text, er, msg_id, None, false,
            );
            Some((formatted, Some(msg_id)))
        } else {
            Some((full_text, None))
        }
    } else {
        None
    };

    let is_http_api = matches!(
        backend.delivery_mode(),
        crate::backend::DeliveryMode::HttpApi { .. }
    );

    crate::backend::claude_code::pre_trust_workspace(&dir);
    crate::backend::pre_trust_mise(&dir);

    let start_result = tokio::task::spawn_blocking({
        let tmux_session = tmux_session.clone();
        let window_name = window_name.clone();
        let pane_credential = session_start_credential.clone();
        let pane_incarnation = owner.incarnation;
        move || -> anyhow::Result<String> {
            use std::process::Command;

            // Name tmux session after project directory (grouping related
            // sessions), and windows after the ouija session name.
            let tmux_session_exists = Command::new("tmux")
                .args(["has-session", "-t", &tmux_session])
                .output()
                .is_ok_and(|o| o.status.success());

            // `pane_env_args` sets OUIJA_SESSION_ID (primary session-id
            // signal for the ouija CLI) plus HISTFILE/fish_history to
            // suppress shell history writes.
            let env_args = crate::tmux::pane_env_args(
                &window_name,
                pane_credential.as_deref(),
                Some(pane_incarnation),
            );
            let pane_id = if tmux_session_exists {
                let target = format!("{tmux_session}:");
                let mut args: Vec<&str> = vec!["new-window", "-d"];
                args.extend(env_args.iter().map(String::as_str));
                args.extend_from_slice(&[
                    "-t",
                    &target,
                    "-n",
                    &window_name,
                    "-P",
                    "-F",
                    "#{pane_id}",
                ]);
                let output = Command::new("tmux").args(&args).output()?;
                if !output.status.success() {
                    anyhow::bail!(
                        "tmux new-window failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            } else {
                let mut args: Vec<&str> = vec!["new-session", "-d"];
                args.extend(env_args.iter().map(String::as_str));
                args.extend_from_slice(&[
                    "-s",
                    &tmux_session,
                    "-n",
                    &window_name,
                    "-P",
                    "-F",
                    "#{pane_id}",
                ]);
                let output = Command::new("tmux").args(&args).output()?;
                if !output.status.success() {
                    anyhow::bail!(
                        "tmux new-session failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            };

            Ok(pane_id)
        }
    })
    .await;

    match start_result {
        Ok(Ok(pane_id)) => {
            match state
                .record_inert_start_pane(&owner, owner.clone(), pane_id.clone())
                .await
            {
                Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
                Ok(outcome) => {
                    let cleanup = remove_inert_start_pane(state, &owner, &pane_id).await;
                    if cleanup.is_ok() {
                        let _ = state.abort_lifecycle(&owner).await;
                    }
                    return (
                        format!(
                            "start for session '{name}' was superseded before pane registration ({outcome:?}){}",
                            cleanup
                                .err()
                                .map(|error| format!("; inert pane cleanup failed: {error}"))
                                .unwrap_or_default()
                        ),
                        None,
                    );
                }
                Err(error) => {
                    let cleanup = remove_inert_start_pane(state, &owner, &pane_id).await;
                    if cleanup.is_ok() {
                        let _ = state.abort_lifecycle(&owner).await;
                    }
                    return (
                        format!(
                            "failed to persist inert pane for session '{name}': {error}{}",
                            cleanup
                                .err()
                                .map(|cleanup_error| {
                                    format!("; inert pane cleanup failed: {cleanup_error}")
                                })
                                .unwrap_or_default()
                        ),
                        None,
                    );
                }
            }

            // Publish the exact reserved owner for every backend before its
            // command can run. Hooks and delayed launch results therefore
            // observe one authoritative incarnation.
            let initial_meta = crate::daemon_protocol::SessionMeta {
                project_dir: Some(dir.clone()),
                worktree,
                backend: Some(backend_name.clone()),
                session_start_credential: session_start_credential.clone(),
                model: model.map(String::from),
                effort: effort.map(String::from),
                codex_home: launch_model.codex_home.clone(),
                reminder: reminder.map(String::from),
                parent_session: parent_session.map(String::from),
                idle_policy: idle_policy.clone(),
                prompt: stored_prompt.map(String::from),
                fresh_context_after_active_secs: active_context_policy_for_launch(
                    None,
                    fresh_context_after_active_secs,
                    true,
                ),
                ..Default::default()
            };
            match Box::pin(state.commit_reserved_start(&owner, Some(pane_id.clone()), initial_meta))
                .await
            {
                Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
                Ok(outcome) => {
                    let cleanup = remove_inert_start_pane(state, &owner, &pane_id).await;
                    if cleanup.is_ok() {
                        let _ = state.abort_lifecycle(&owner).await;
                    }
                    return (
                        format!(
                            "start for session '{name}' was superseded before launch commit ({outcome:?}){}",
                            cleanup
                                .err()
                                .map(|error| format!("; inert pane cleanup failed: {error}"))
                                .unwrap_or_default()
                        ),
                        None,
                    );
                }
                Err(error) => {
                    let cleanup = remove_inert_start_pane(state, &owner, &pane_id).await;
                    if cleanup.is_ok() {
                        let _ = state.abort_lifecycle(&owner).await;
                    }
                    return (
                        format!(
                            "failed to persist session '{name}' before launch: {error}{}",
                            cleanup
                                .err()
                                .map(|cleanup_error| {
                                    format!("; inert pane cleanup failed: {cleanup_error}")
                                })
                                .unwrap_or_default()
                        ),
                        None,
                    );
                }
            }

            let pane_for_launch = pane_id.clone();
            let mut prepared_command = match prepare_backend_launch_command(
                is_http_api,
                &backend_cmd,
                pre_queued_prompt
                    .as_ref()
                    .map(|(prompt_text, _)| prompt_text.as_str()),
            ) {
                Ok(command) => command,
                Err(error) => {
                    cleanup_reserved_start(
                        state,
                        &owner,
                        &pane_id,
                        session_start_credential.as_deref(),
                    )
                    .await;
                    return (
                        format!("start failed to prepare launch prompt: {error}"),
                        None,
                    );
                }
            };
            let command = if is_http_api {
                prepared_command.command().to_string()
            } else {
                crate::tmux::close_shell_after(prepared_command.command())
            };
            let launch_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                crate::tmux::configure_managed_pane(&pane_for_launch);
                // Leading space keeps the command out of shell history
                // (fallback for shells that honour HIST_IGNORE_SPACE).
                let hidden_cmd = format!(" {command}");
                let status = std::process::Command::new("tmux")
                    .args(["send-keys", "-t", &pane_for_launch, &hidden_cmd, "Enter"])
                    .status()?;
                if !status.success() {
                    anyhow::bail!("tmux send-keys failed for pane {pane_for_launch}");
                }
                prepared_command.mark_handed_off();
                Ok(())
            })
            .await;
            match launch_result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    cleanup_reserved_start(
                        state,
                        &owner,
                        &pane_id,
                        session_start_credential.as_deref(),
                    )
                    .await;
                    return (format!("start failed: {error}"), None);
                }
                Err(error) => {
                    cleanup_reserved_start(
                        state,
                        &owner,
                        &pane_id,
                        session_start_credential.as_deref(),
                    )
                    .await;
                    return (format!("start failed: {error}"), None);
                }
            }

            // For HttpApi backends, use the shared opencode serve instance
            let is_http_api = matches!(
                backend.delivery_mode(),
                crate::backend::DeliveryMode::HttpApi { .. }
            );
            let backend_session_id = if is_http_api {
                match setup_shared_serve_session(state, &owner, None, &pane_id, &dir).await {
                    Ok(sid) => Some(sid),
                    Err(e) => {
                        tracing::warn!("shared serve session setup failed: {e}");
                        None
                    }
                }
            } else {
                None
            };

            let Some((registration_pane, backend_session_id, opencode_binding)) =
                start_registration_metadata(is_http_api, &pane_id, backend_session_id)
            else {
                tracing::warn!(
                    "start_session: not registering {name} because OpenCode attach setup failed"
                );
                if should_cleanup_failed_opencode_attach_pane(is_http_api, None) {
                    cleanup_reserved_start(
                        state,
                        &owner,
                        &pane_id,
                        session_start_credential.as_deref(),
                    )
                    .await;
                }
                return (
                    format!(
                        "start failed: OpenCode attach setup failed for '{name}' (pane {pane_id})"
                    ),
                    None,
                );
            };

            let pending_session_start_credential = session_start_credential.clone();
            let oc_session_id = backend_session_id.clone();
            let proto_meta = crate::daemon_protocol::SessionMeta {
                project_dir: Some(dir.clone()),
                worktree,
                backend: Some(backend_name.clone()),
                session_start_credential: pending_session_start_credential,
                backend_session_id,
                opencode_binding,
                model: model.map(String::from),
                effort: effort.map(String::from),
                codex_home: launch_model.codex_home.clone(),
                reminder: reminder.map(String::from),
                parent_session: parent_session.map(String::from),
                idle_policy,
                prompt: stored_prompt.map(String::from),
                fresh_context_after_active_secs: active_context_policy_for_launch(
                    None,
                    fresh_context_after_active_secs,
                    true,
                ),
                ..Default::default()
            };
            match finalize_reserved_start(state, &owner, registration_pane, proto_meta).await {
                Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
                Ok(outcome) => {
                    return (
                        format!(
                            "start for session '{name}' was superseded during finalization ({outcome:?})"
                        ),
                        None,
                    );
                }
                Err(error) => {
                    cleanup_reserved_start(
                        state,
                        &owner,
                        &pane_id,
                        session_start_credential.as_deref(),
                    )
                    .await;
                    return (
                        format!("failed to persist final metadata for session '{name}': {error}"),
                        None,
                    );
                }
            }
            if let Err(error) = state.abort_lifecycle(&owner).await {
                let cleanup = remove_inert_start_pane(state, &owner, &pane_id).await;
                cleanup_reserved_start(
                    state,
                    &owner,
                    &pane_id,
                    session_start_credential.as_deref(),
                )
                .await;
                return (
                    format!(
                        "start failed to persist launch completion: {error}{}",
                        cleanup
                            .err()
                            .map(|cleanup_error| {
                                format!("; launched pane cleanup failed: {cleanup_error}")
                            })
                            .unwrap_or_default()
                    ),
                    None,
                );
            }
            let prompt_delivery = pre_queued_prompt
                .as_ref()
                .map(|_| start_prompt_delivery(is_http_api, oc_session_id.as_deref()));
            let prompt_msg_id = start_prompt_msg_id(
                pre_queued_prompt.as_ref().and_then(|(_, id)| *id),
                prompt_delivery,
            );
            if let Some((ref prompt_text, _)) = pre_queued_prompt {
                match prompt_delivery.expect("prompt delivery is computed when prompt exists") {
                    StartPromptDelivery::PromptAsync => {
                        let oc_sid = oc_session_id
                            .as_ref()
                            .expect("PromptAsync delivery requires an OpenCode backend session id");
                        let port = state.opencode_serve_port();
                        let body = opencode_prompt_body(prompt_text, model, effort);
                        let url = format!("http://127.0.0.1:{port}/session/{oc_sid}/prompt_async");
                        let state2 = state.clone();
                        let dir2 = dir.clone();
                        let name2 = name.to_string();
                        let pane2 = pane_id.clone();
                        let expected_backend_session_id = oc_session_id.clone();
                        let injected = prompt_text.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                            let resp = state2
                                .http_client
                                .post(&url)
                                .header("x-opencode-directory", &dir2)
                                .json(&body)
                                .timeout(std::time::Duration::from_secs(10))
                                .send()
                                .await;
                            match resp {
                                Ok(r) if r.status().is_success() => {
                                    tracing::info!(
                                        "start_session: delivered prompt to {name2} via prompt_async"
                                    );
                                }
                                Ok(r) => {
                                    let status = r.status();
                                    tracing::warn!("start_session: prompt_async returned {status}");
                                    let decision = classify_prompt_async_fallback(
                                        PromptAsyncFailure::Status(status),
                                    );
                                    if decision.should_try_raw_tmux() {
                                        if deliver_prompt_fallback(
                                            &state2,
                                            &name2,
                                            &pane2,
                                            &injected,
                                            true,
                                            false,
                                            expected_backend_session_id.as_deref(),
                                            None,
                                        )
                                        .await
                                        .is_err()
                                        {
                                            restore_start_prompt_after_fallback_failure(
                                                &state2,
                                                &name2,
                                                crate::state::PendingPrompt::new(
                                                    pane2.clone(),
                                                    injected.clone(),
                                                    expected_backend_session_id.clone(),
                                                ),
                                            );
                                        }
                                    } else {
                                        tracing::warn!(
                                            "start_session: prompt_async status {status} is ambiguous; not retrying prompt via raw tmux"
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("start_session: prompt_async failed: {e}");
                                    let decision = classify_prompt_async_fallback(
                                        PromptAsyncFailure::Request(&e),
                                    );
                                    if decision.should_try_raw_tmux()
                                        && deliver_prompt_fallback(
                                            &state2,
                                            &name2,
                                            &pane2,
                                            &injected,
                                            true,
                                            false,
                                            expected_backend_session_id.as_deref(),
                                            None,
                                        )
                                        .await
                                        .is_err()
                                    {
                                        restore_start_prompt_after_fallback_failure(
                                            &state2,
                                            &name2,
                                            crate::state::PendingPrompt::new(
                                                pane2.clone(),
                                                injected.clone(),
                                                expected_backend_session_id.clone(),
                                            ),
                                        );
                                    } else if !decision.should_try_raw_tmux() {
                                        tracing::warn!(
                                            "start_session: prompt_async request failure is ambiguous; not retrying prompt via raw tmux"
                                        );
                                    }
                                }
                            }
                        });
                    }
                    StartPromptDelivery::AlreadyPassedAsCliArg => {
                        // TuiInjection prompts are passed as CLI args before spawn.
                    }
                    StartPromptDelivery::Unavailable => {
                        tracing::warn!(
                            "start_session: prompt for {name} not delivered because OpenCode attach setup failed"
                        );
                    }
                }
            }
            if auto_worktree {
                let conflict_name = {
                    let proto = state.protocol.read().await;
                    proto
                        .sessions
                        .values()
                        .find(|s| {
                            s.id != name && s.metadata.project_dir.as_deref() == Some(dir.as_str())
                        })
                        .map(|s| s.id.clone())
                        .unwrap_or_default()
                };
                (
                    format!(
                        "started '{name}' in {dir} (pane {pane_id}, worktree: auto-enabled — session '{conflict_name}' shares this directory)"
                    ),
                    prompt_msg_id,
                )
            } else {
                (
                    format!("started '{name}' in {dir} (pane {pane_id})"),
                    prompt_msg_id,
                )
            }
        }
        Ok(Err(e)) => fail_reserved_start(state, &owner, format!("start failed: {e}")).await,
        Err(e) => fail_reserved_start(state, &owner, format!("start failed: {e}")).await,
    }
}

/// The terminal result of a restart attempt. Callers that coordinate other
/// state transitions must use this typed value rather than infer success from
/// the human-facing status message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartOutcome {
    Restarted,
    Failed,
    Superseded,
}

#[cfg(test)]
async fn claim_restart_for_external_work(
    state: &std::sync::Arc<AppState>,
    name: &str,
) -> Result<crate::daemon_protocol::ResourceOwner, RestartOutcome> {
    let owner = {
        let protocol = state.protocol.read().await;
        let Some(session) = protocol.sessions.get(name) else {
            return Err(RestartOutcome::Failed);
        };
        if !matches!(session.origin, crate::daemon_protocol::Origin::Local) {
            return Err(RestartOutcome::Failed);
        }
        session.owner()
    };
    match state.claim_existing_start(&owner).await {
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => Ok(owner),
        Ok(_) => Err(RestartOutcome::Superseded),
        Err(error) => {
            tracing::warn!(session = name, "failed to persist restart claim: {error}");
            Err(RestartOutcome::Failed)
        }
    }
}

fn restart_recovery_pending(
    proto: &crate::daemon_protocol::DaemonState,
    owner: &crate::daemon_protocol::ResourceOwner,
) -> bool {
    proto
        .lifecycle_leases
        .get(&owner.session_id)
        .is_some_and(|lease| {
            lease.owner == *owner
                && lease.phase == crate::daemon_protocol::LifecyclePhase::Restarting
                && (lease.inert_pane.is_some()
                    || proto
                        .sessions
                        .get(&owner.session_id)
                        .is_some_and(|session| {
                            session.metadata.session_incarnation != lease.owner.incarnation
                        }))
        })
}

fn select_restart_resume_id(
    fresh: bool,
    selected_backend: &str,
    previous_metadata: Option<&crate::daemon_protocol::SessionMeta>,
    detected_session_id: Option<String>,
) -> Option<String> {
    let previous_backend_matches = previous_metadata
        .and_then(|metadata| metadata.backend.as_deref())
        == Some(selected_backend);
    if fresh || !previous_backend_matches {
        return None;
    }
    previous_metadata
        .and_then(|metadata| metadata.backend_session_id.clone())
        .or(detected_session_id)
}

fn previous_http_restart_fallback_id(
    fresh: bool,
    selected_backend: &str,
    previous_metadata: Option<&crate::daemon_protocol::SessionMeta>,
) -> Option<String> {
    select_restart_resume_id(fresh, selected_backend, previous_metadata, None)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HttpRestartBackendPlan {
    Reuse(String),
    Create,
}

fn select_http_restart_backend_plan(
    is_http_api: bool,
    fresh: bool,
    selected_backend: &str,
    previous_metadata: Option<&crate::daemon_protocol::SessionMeta>,
) -> Option<HttpRestartBackendPlan> {
    if !is_http_api {
        return None;
    }

    Some(
        previous_http_restart_fallback_id(fresh, selected_backend, previous_metadata)
            .map(HttpRestartBackendPlan::Reuse)
            .unwrap_or(HttpRestartBackendPlan::Create),
    )
}

/// Recover an interactive fresh launch that definitely failed before the
/// backend command could start. The protocol guards make this a no-op when a
/// concurrent SessionStart has already consumed the credential and bound the
/// new backend identity.
#[cfg(test)]
async fn recover_failed_fresh_launch(
    state: &std::sync::Arc<AppState>,
    id: &str,
    pane: Option<String>,
    credential: Option<String>,
    staged_incarnation: Option<crate::daemon_protocol::SessionIncarnation>,
    previous: Option<crate::daemon_protocol::SessionEntry>,
    provisional_pane: Option<String>,
) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
    let Some(staged_incarnation) = staged_incarnation else {
        return Ok(crate::daemon_protocol::LifecycleMutationOutcome::NotFound);
    };
    let owner = crate::daemon_protocol::ResourceOwner {
        session_id: id.to_string(),
        incarnation: staged_incarnation,
    };
    if let Some(ref provisional_pane) = provisional_pane {
        remove_inert_start_pane(state, &owner, provisional_pane).await?;
    }
    state
        .rollback_launch(
            &owner,
            pane.as_deref(),
            credential.as_deref(),
            previous,
            provisional_pane.as_deref(),
        )
        .await
}

async fn rollback_claimed_restart(
    state: &std::sync::Arc<AppState>,
    lease_owner: &crate::daemon_protocol::ResourceOwner,
    target_owner: &crate::daemon_protocol::ResourceOwner,
    provisional_pane: Option<&str>,
) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
    if restart_backend_cleanup_pending(state, lease_owner, target_owner).await {
        anyhow::bail!("restart backend cleanup is still pending for target {target_owner:?}");
    }
    state
        .rollback_restart_launch(lease_owner, target_owner, provisional_pane)
        .await
}

/// Execute `/start`'s existing-session restart only while the synchronously
/// claimed incumbent still owns both the session and its durable restart
/// lease. The claim is released after every terminal result.
#[allow(clippy::too_many_arguments)]
pub async fn restart_session_for_start(
    state: &std::sync::Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
    name: &str,
    fresh: bool,
    repair_reservation: Option<crate::daemon_protocol::BackendRepairReservation>,
    prompt: Option<&str>,
    from: Option<&str>,
    expects_reply: Option<bool>,
    backend: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    reminder: Option<&str>,
    parent_session_override: ParentSessionOverride,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
) -> (String, Option<u64>, RestartOutcome) {
    restart_session_for_start_with_active_context_policy(
        state,
        owner,
        name,
        fresh,
        None,
        repair_reservation,
        prompt,
        from,
        expects_reply,
        backend,
        model,
        effort,
        reminder,
        parent_session_override,
        idle_policy,
    )
    .await
}

/// Restart a claimed session with an API-validated active-context policy.
#[allow(clippy::too_many_arguments)]
pub async fn restart_session_for_start_with_active_context_policy(
    state: &std::sync::Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
    name: &str,
    fresh: bool,
    fresh_context_after_active_secs: Option<u64>,
    repair_reservation: Option<crate::daemon_protocol::BackendRepairReservation>,
    prompt: Option<&str>,
    from: Option<&str>,
    expects_reply: Option<bool>,
    backend: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    reminder: Option<&str>,
    parent_session_override: ParentSessionOverride,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
) -> (String, Option<u64>, RestartOutcome) {
    restart_session_for_start_with_prompt_controls(
        state,
        owner,
        name,
        fresh,
        fresh_context_after_active_secs,
        repair_reservation,
        prompt,
        false,
        None,
        from,
        expects_reply,
        backend,
        model,
        effort,
        reminder,
        parent_session_override,
        idle_policy,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn restart_session_for_start_with_prompt_controls(
    state: &std::sync::Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
    name: &str,
    fresh: bool,
    fresh_context_after_active_secs: Option<u64>,
    repair_reservation: Option<crate::daemon_protocol::BackendRepairReservation>,
    prompt: Option<&str>,
    suppress_stored_prompt: bool,
    one_shot_prompt: Option<&str>,
    from: Option<&str>,
    expects_reply: Option<bool>,
    backend: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    reminder: Option<&str>,
    parent_session_override: ParentSessionOverride,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
) -> (String, Option<u64>, RestartOutcome) {
    let still_claimed = {
        let proto = state.protocol.read().await;
        proto.sessions.get(name).is_some_and(|session| {
            matches!(session.origin, crate::daemon_protocol::Origin::Local)
                && session.metadata.session_incarnation == owner.incarnation
        }) && proto.lifecycle_leases.get(name).is_some_and(|lease| {
            lease.owner == *owner
                && lease.phase == crate::daemon_protocol::LifecyclePhase::Restarting
        })
    };
    if !still_claimed || owner.session_id != name {
        let _ = state.abort_lifecycle(owner).await;
        return (
            "restart superseded before external work".into(),
            None,
            RestartOutcome::Superseded,
        );
    }

    let result = restart_session_claimed(
        state,
        owner,
        name,
        fresh,
        fresh_context_after_active_secs,
        repair_reservation,
        prompt,
        suppress_stored_prompt,
        one_shot_prompt,
        from,
        expects_reply,
        backend,
        model,
        effort,
        reminder,
        parent_session_override,
        idle_policy,
    )
    .await;
    let retain_recovery_lease = result.2 == RestartOutcome::Failed && {
        let proto = state.protocol.read().await;
        restart_recovery_pending(&proto, owner)
    };
    if retain_recovery_lease {
        return (
            format!("{}; durable restart recovery authority retained", result.0),
            result.1,
            result.2,
        );
    }
    if let Err(error) = state.abort_lifecycle(owner).await {
        return (
            format!(
                "{}; failed to release existing-session restart claim: {error}",
                result.0
            ),
            result.1,
            RestartOutcome::Failed,
        );
    }
    result
}

/// Kill and restart a session, preserving metadata unless `fresh`.
#[allow(clippy::too_many_arguments)]
pub async fn restart_session(
    state: &std::sync::Arc<AppState>,
    name: &str,
    fresh: bool,
    repair_reservation: Option<crate::daemon_protocol::BackendRepairReservation>,
    prompt: Option<&str>,
    from: Option<&str>,
    expects_reply: Option<bool>,
    backend: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    reminder: Option<&str>,
    parent_session_override: ParentSessionOverride,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
) -> (String, Option<u64>, RestartOutcome) {
    restart_session_with_prompt_controls(
        state,
        name,
        fresh,
        None,
        repair_reservation,
        prompt,
        false,
        None,
        from,
        expects_reply,
        backend,
        model,
        effort,
        reminder,
        parent_session_override,
        idle_policy,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn restart_session_with_prompt_controls(
    state: &std::sync::Arc<AppState>,
    name: &str,
    fresh: bool,
    fresh_context_after_active_secs: Option<u64>,
    repair_reservation: Option<crate::daemon_protocol::BackendRepairReservation>,
    prompt: Option<&str>,
    suppress_stored_prompt: bool,
    one_shot_prompt: Option<&str>,
    from: Option<&str>,
    expects_reply: Option<bool>,
    backend: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    reminder: Option<&str>,
    parent_session_override: ParentSessionOverride,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
) -> (String, Option<u64>, RestartOutcome) {
    let disposition = match reserve_start_for_launch(state, name).await {
        Ok(disposition) => disposition,
        Err(error) => {
            tracing::warn!(
                session = name,
                "failed to persist restart reservation: {error}"
            );
            return (
                format!("failed to reserve restart for '{name}'"),
                None,
                RestartOutcome::Failed,
            );
        }
    };
    let owner = match disposition {
        crate::daemon_protocol::StartDisposition::Reserved(owner) => {
            if repair_reservation.is_some() {
                let _ = state.abort_lifecycle(&owner).await;
                return (
                    "restart repair target disappeared before staging".into(),
                    None,
                    RestartOutcome::Superseded,
                );
            }
            let parent_session = parent_session_override.resolve(None);
            let resolved_prompt = resolve_restart_prompt(
                None,
                RestartPromptInput {
                    replacement: prompt,
                    suppress_stored: suppress_stored_prompt,
                    one_shot: one_shot_prompt,
                },
            );
            let result = start_session_with_prompt_storage(
                state,
                name,
                None,
                None,
                resolved_prompt.launch.as_deref(),
                resolved_prompt.stored.as_deref(),
                from,
                expects_reply,
                backend,
                model,
                effort,
                reminder,
                parent_session.as_deref(),
                idle_policy,
                None,
                None,
                false,
                fresh_context_after_active_secs,
                Some(owner.clone()),
            )
            .await;
            let protocol = state.protocol.read().await;
            let outcome = match protocol.sessions.get(name) {
                Some(session) if session.owner() == owner => RestartOutcome::Restarted,
                Some(_) => RestartOutcome::Superseded,
                None => RestartOutcome::Failed,
            };
            return (result.0, result.1, outcome);
        }
        crate::daemon_protocol::StartDisposition::InProgress(_) => {
            return (
                format!("restart already in progress for '{name}'"),
                None,
                RestartOutcome::Superseded,
            );
        }
        crate::daemon_protocol::StartDisposition::Existing(owner) => {
            match state.claim_existing_start(&owner).await {
                Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => owner,
                Ok(_) => {
                    return (
                        format!("restart superseded while claiming '{name}'"),
                        None,
                        RestartOutcome::Superseded,
                    );
                }
                Err(error) => {
                    tracing::warn!(session = name, "failed to persist restart claim: {error}");
                    return (
                        format!("failed to claim restart for '{name}'"),
                        None,
                        RestartOutcome::Failed,
                    );
                }
            }
        }
    };
    restart_session_for_start_with_prompt_controls(
        state,
        &owner,
        name,
        fresh,
        fresh_context_after_active_secs,
        repair_reservation,
        prompt,
        suppress_stored_prompt,
        one_shot_prompt,
        from,
        expects_reply,
        backend,
        model,
        effort,
        reminder,
        parent_session_override,
        idle_policy,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn restart_session_claimed(
    state: &std::sync::Arc<AppState>,
    lease_owner: &crate::daemon_protocol::ResourceOwner,
    name: &str,
    fresh: bool,
    fresh_context_after_active_secs: Option<u64>,
    repair_reservation: Option<crate::daemon_protocol::BackendRepairReservation>,
    prompt: Option<&str>,
    suppress_stored_prompt: bool,
    one_shot_prompt: Option<&str>,
    from: Option<&str>,
    expects_reply: Option<bool>,
    backend: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    reminder: Option<&str>,
    parent_session_override: ParentSessionOverride,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
) -> (String, Option<u64>, RestartOutcome) {
    // Snapshot only while the exact incumbent and durable restart claim still
    // agree. Every public caller claims this authority before reaching any
    // HTTP, tmux, process, attach, readiness, or prompt work.
    let session = {
        let protocol = state.protocol.read().await;
        let lease_matches = protocol.lifecycle_leases.get(name).is_some_and(|lease| {
            lease.owner == *lease_owner
                && lease.phase == crate::daemon_protocol::LifecyclePhase::Restarting
        });
        protocol
            .sessions
            .get(name)
            .filter(|session| lease_matches && session.owner() == *lease_owner)
            .cloned()
    };
    if session.is_none() {
        return (
            "restart superseded before external work".into(),
            None,
            RestartOutcome::Superseded,
        );
    }
    // Snapshot full metadata before staging so we can carry it forward.
    let prev_metadata = session.as_ref().map(|s| s.metadata.clone());
    if let Some(expected) = repair_reservation.as_ref()
        && !session.as_ref().is_some_and(|session| {
            session.metadata.backend_repair_reservation.as_ref() == Some(expected)
                && expected.phase == crate::daemon_protocol::BackendRepairPhase::PreStage
                && session.metadata.session_incarnation == expected.original_incarnation
        })
    {
        return (
            "restart superseded before staging repair".into(),
            None,
            RestartOutcome::Superseded,
        );
    }
    // Capture the incumbent pane before staging so a verified live owner can
    // be respawned in place. Recovery clears this to select the recreate path.
    let mut existing_pane = session.as_ref().and_then(|s| s.pane.clone());

    let backend = match backend {
        Some(b) => match state.backends.get_required(b) {
            Ok(backend) => backend,
            Err(message) => return (message, None, RestartOutcome::Failed),
        },
        None => {
            // Fall back to the existing session's backend
            let prev_backend = prev_metadata.as_ref().and_then(|m| m.backend.as_deref());
            match prev_backend {
                Some(b) => state
                    .backends
                    .get(b)
                    .unwrap_or_else(|| state.backends.default()),
                None => state.backends.default(),
            }
        }
    };

    // Preserve previous model/effort when caller omits them, matching the
    // backend fallback logic above. This ensures `ouija restart-session` does
    // not silently downgrade a session to the backend's default model.
    //
    // Treat empty/whitespace-only strings (whether from the caller or from
    // persisted SessionMeta written by an older build) as absent. The API
    // boundary normalizes; this is a belt-and-braces guard so an empty string
    // here never reaches the backend as `claude --model ''` or
    // `variant: ""`.
    // Reuse the API-boundary normalizer so "  sonnet  " trims to "sonnet"
    // instead of flowing through to `claude --model '  sonnet  '`. Covers
    // both caller-supplied values and persisted SessionMeta.model/effort
    // from older builds that predate the boundary normalization.
    let caller_model = crate::api::normalize_optional_string(model.map(String::from));
    let effective_model = caller_model.clone().or_else(|| {
        crate::api::normalize_optional_string(prev_metadata.as_ref().and_then(|m| m.model.clone()))
    });
    let effective_effort =
        crate::api::normalize_optional_string(effort.map(String::from)).or_else(|| {
            crate::api::normalize_optional_string(
                prev_metadata.as_ref().and_then(|m| m.effort.clone()),
            )
        });
    let effective_manual_reminder = match &prev_metadata {
        Some(m) => reminder.map(String::from).or_else(|| m.reminder.clone()),
        None => reminder.map(String::from),
    };
    let effective_parent_session = parent_session_override.resolve(prev_metadata.as_ref());
    let effective_idle_policy = match &prev_metadata {
        Some(m) => idle_policy.clone().or_else(|| m.idle_policy.clone()),
        None => idle_policy.clone(),
    };
    let resolved_prompt = resolve_restart_prompt(
        prev_metadata
            .as_ref()
            .and_then(|metadata| metadata.prompt.as_deref()),
        RestartPromptInput {
            replacement: prompt,
            suppress_stored: suppress_stored_prompt,
            one_shot: one_shot_prompt,
        },
    );
    let projects_dir = state.settings.read().await.projects_dir.clone();
    let base = match projects_dir {
        Some(dir) => crate::state::expand_tilde(&dir),
        None => crate::state::expand_tilde("~/code"),
    };
    let dir = prev_metadata
        .as_ref()
        .and_then(|m| m.project_dir.clone())
        .unwrap_or_else(|| format!("{base}/{name}"));
    let backend_name = backend.name().to_string();
    let detected_session_id = (!fresh
        && prev_metadata
            .as_ref()
            .and_then(|metadata| metadata.backend.as_deref())
            == Some(backend_name.as_str()))
    .then(|| backend.detect_session_id(&dir))
    .flatten();
    let resume_id = select_restart_resume_id(
        fresh,
        &backend_name,
        prev_metadata.as_ref(),
        detected_session_id,
    );
    if let Some(ref sid) = resume_id {
        tracing::info!("restart '{name}': using --resume {sid}");
    }
    let launches_new_backend_identity = fresh || resume_id.is_none();
    let session_start_credential = (backend_name == "codex-cli" && launches_new_backend_identity)
        .then(crate::daemon_protocol::new_session_start_credential);

    let staged_incarnation = match state
        .stage_restart_launch(
            lease_owner,
            backend_name.clone(),
            launches_new_backend_identity,
            fresh,
            fresh_context_after_active_secs,
            session_start_credential.clone(),
            repair_reservation.clone(),
        )
        .await
    {
        crate::daemon_protocol::StageFreshLaunchOutcome::Staged { incarnation } => {
            Some(incarnation)
        }
        crate::daemon_protocol::StageFreshLaunchOutcome::Rejected => {
            return (
                "restart superseded before external work".into(),
                None,
                RestartOutcome::Superseded,
            );
        }
        crate::daemon_protocol::StageFreshLaunchOutcome::PersistenceFailed => {
            return (
                "restart failed to persist target lifecycle authority".into(),
                None,
                RestartOutcome::Failed,
            );
        }
    };
    let restart_target_owner = crate::daemon_protocol::ResourceOwner {
        session_id: name.to_string(),
        incarnation: staged_incarnation.expect("staged restart must have a target incarnation"),
    };

    // --- Soft restart for HttpApi backends ---
    // Create a new session on the serve via HTTP API and deliver the prompt directly.
    // No tmux interaction needed — the LLM works in the serve, not the TUI.
    if fresh {
        let is_http_api = matches!(
            backend.delivery_mode(),
            crate::backend::DeliveryMode::HttpApi { .. }
        );
        if is_http_api {
            let dir = prev_metadata
                .as_ref()
                .and_then(|m| m.project_dir.clone())
                .unwrap_or_default();
            if let Ok(result) = soft_restart_session_claimed(
                state,
                lease_owner,
                &restart_target_owner,
                prev_metadata
                    .as_ref()
                    .expect("claimed restart must retain previous metadata"),
                name,
                existing_pane.as_deref(),
                &dir,
                resolved_prompt.launch.as_deref(),
                prompt,
                fresh_context_after_active_secs,
                from,
                expects_reply,
                effective_manual_reminder.as_deref(),
                parent_session_override.clone(),
                effective_idle_policy.clone(),
                effective_model.as_deref(),
                effective_effort.as_deref(),
            )
            .await
            {
                // soft_restart_session writes backend_session_id + model +
                // effort atomically under one lock before delivering, so the
                // caller does not need a second write here.
                return (result.0, result.1, RestartOutcome::Restarted);
            }
            if restart_backend_cleanup_pending(state, lease_owner, &restart_target_owner).await {
                return (
                    format!(
                        "soft restart failed for '{name}' with durable backend cleanup pending"
                    ),
                    None,
                    RestartOutcome::Failed,
                );
            }
            tracing::info!("soft restart failed for '{name}', falling back to hard restart");
        }
    }

    // No Remove before restart: keep the session in state so that
    // inherit_recurrence_from preserves metadata (prompt, reminder).
    // The subsequent Register re-registers in place — apply_register handles
    // old pane cleanup and agent restart when the pane changes.
    //
    // Refresh registered_at so the reaper's 60s grace period protects the
    // session during the brief window when pane_alive returns false (old
    // process dead, new one not yet started).
    {
        let mut proto = state.protocol.write().await;
        if let Some(s) = proto.sessions.get_mut(name) {
            s.registered_at = chrono::Utc::now().timestamp();
        }
    }

    // Ouija manages worktrees in .ouija/worktrees/ — the backend just gets a dir.
    // On restart, the worktree already exists (project_dir points to it).

    crate::backend::claude_code::pre_trust_workspace(&dir);
    crate::backend::pre_trust_mise(&dir);

    let settings = state.settings.read().await;
    let claude_permission_mode = settings.claude_permission_mode.clone();
    let launch_model = crate::backend::resolve_launch_model_config(
        &backend_name,
        effective_model.clone(),
        &settings,
    );
    drop(settings);
    let launch_codex_home = if caller_model.is_some() {
        launch_model.codex_home.clone()
    } else {
        launch_model
            .codex_home
            .clone()
            .or_else(|| prev_metadata.as_ref().and_then(|m| m.codex_home.clone()))
    };
    crate::backend::codex::install_configured_home(launch_codex_home.as_deref());

    let claude_cmd = if fresh {
        backend.build_start_command(&crate::backend::StartOpts {
            project_dir: dir.clone(),
            worktree: None, // ouija manages worktrees, not the backend
            model: launch_model.model.clone(),
            effort: effective_effort.clone(),
            permission_mode: claude_permission_mode,
            codex_home: launch_codex_home.clone(),
        })
    } else {
        backend
            .build_resume_command(&crate::backend::ResumeOpts {
                project_dir: dir.clone(),
                session_id: resume_id.clone(),
                worktree: None, // ouija manages worktrees
                model: launch_model.model.clone(),
                effort: effective_effort.clone(),
                permission_mode: claude_permission_mode.clone(),
                codex_home: launch_codex_home.clone(),
            })
            .unwrap_or_else(|| {
                backend.build_start_command(&crate::backend::StartOpts {
                    project_dir: dir.clone(),
                    worktree: None,
                    model: launch_model.model.clone(),
                    effort: effective_effort.clone(),
                    permission_mode: claude_permission_mode.clone(),
                    codex_home: launch_codex_home.clone(),
                })
            })
    };

    // Prompt resolution is computed exactly once above and shared by both the
    // OpenCode soft path and this hard fallback.
    let effective_prompt = resolved_prompt.launch.clone();
    let reminder_meta = crate::daemon_protocol::SessionMeta {
        reminder: effective_manual_reminder.clone(),
        parent_session: effective_parent_session.clone(),
        idle_policy: effective_idle_policy.clone(),
        ..Default::default()
    };
    let effective_reminder = reminder_meta.effective_reminder(name, None);

    // Format prompt text with sender envelope if needed
    let (formatted_prompt, prompt_msg_id) = if let Some(ref text) = effective_prompt {
        let full_text = match &effective_reminder {
            Some(r) => format!("{text}\n\n{r}"),
            None => text.clone(),
        };
        if let Some(sender) = from {
            let er = expects_reply.unwrap_or(true);
            let msg_id = {
                let mut proto = state.protocol.write().await;
                proto.next_seq()
            };
            (
                Some(crate::daemon_protocol::format_session_message(
                    sender, &full_text, er, msg_id, None, false,
                )),
                Some(msg_id),
            )
        } else {
            (Some(full_text), None)
        }
    } else {
        (None, None)
    };

    let tmux_session = crate::tmux::tmux_session_name(&dir);
    let window_name = name.to_string();
    let is_http_api = matches!(
        backend.delivery_mode(),
        crate::backend::DeliveryMode::HttpApi { .. }
    );

    // Direct respawn recovery can safely recognize only managed panes that
    // already export the incumbent or staged incarnation. A missing pane can
    // use the new-window fallback; refuse live legacy/unverified panes before
    // staging a replacement.
    if let Some(incumbent_pane_id) = existing_pane.clone() {
        let pane = incumbent_pane_id.clone();
        let inspected =
            tokio::task::spawn_blocking(move || crate::tmux::inspect_managed_pane(&pane)).await;
        let verification_error = match inspected {
            Ok(Ok(inspection)) => {
                match classify_incumbent_pane(&inspection, lease_owner, &restart_target_owner) {
                    IncumbentPaneDisposition::Respawn => None,
                    IncumbentPaneDisposition::Recreate => {
                        existing_pane = None;
                        None
                    }
                    IncumbentPaneDisposition::Refuse => {
                        let live_owner = inspection.owner();
                        Some(format!(
                            "restart refused unverified incumbent pane {incumbent_pane_id}: expected {lease_owner:?} or {restart_target_owner:?}, found {live_owner:?}"
                        ))
                    }
                }
            }
            Ok(Err(error)) => Some(format!(
                "restart could not verify incumbent pane {incumbent_pane_id}: {error}"
            )),
            Err(error) => Some(format!(
                "restart incumbent pane verification task failed for {incumbent_pane_id}: {error}"
            )),
        };
        if let Some(error) = verification_error {
            let _ = rollback_claimed_restart(state, lease_owner, &restart_target_owner, None).await;
            return (error, None, RestartOutcome::Failed);
        }
    }

    // A fresh direct respawn repurposes an existing pane in place. Record the
    // pane and its staged owner before `respawn-pane`; restore accepts either
    // the incumbent (crash before replacement) or staged owner (crash after
    // replacement) and removes only that exact managed pane.
    let direct_restart_lease_owner = if let (Some(existing_pane), Some(expected_incarnation)) =
        (existing_pane.as_ref(), staged_incarnation)
    {
        let pane_owner = crate::daemon_protocol::ResourceOwner {
            session_id: name.to_string(),
            incarnation: expected_incarnation,
        };
        match state
            .record_inert_start_pane(lease_owner, pane_owner, existing_pane.clone())
            .await
        {
            Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
            Ok(outcome) => {
                let _ =
                    rollback_claimed_restart(state, lease_owner, &restart_target_owner, None).await;
                return (
                    format!("restart superseded before direct respawn ({outcome:?})"),
                    None,
                    RestartOutcome::Superseded,
                );
            }
            Err(error) => {
                let rollback =
                    rollback_claimed_restart(state, lease_owner, &restart_target_owner, None).await;
                return (
                    format!(
                        "restart failed to persist direct respawn authority: {error}{}",
                        rollback
                            .err()
                            .map(|rollback_error| {
                                format!("; durable rollback failed: {rollback_error}")
                            })
                            .unwrap_or_default()
                    ),
                    None,
                    RestartOutcome::Failed,
                );
            }
        }
        Some(lease_owner.clone())
    } else {
        None
    };

    let claude_cmd = match session_start_credential.as_deref() {
        Some(credential) => {
            let Some(incarnation) = staged_incarnation else {
                return (
                    "restart missing staged Codex incarnation".into(),
                    None,
                    RestartOutcome::Failed,
                );
            };
            match crate::backend::codex::with_session_start_hook(
                claude_cmd,
                launch_codex_home.as_deref(),
                name,
                credential,
                incarnation,
            ) {
                Ok(command) => command,
                Err(error) => {
                    let rollback =
                        rollback_claimed_restart(state, lease_owner, &restart_target_owner, None)
                            .await;
                    return (
                        format!(
                            "could not stage Codex launch credential: {error}{}",
                            rollback
                                .err()
                                .map(|rollback_error| {
                                    format!("; durable rollback failed: {rollback_error}")
                                })
                                .unwrap_or_default()
                        ),
                        None,
                        RestartOutcome::Failed,
                    );
                }
            }
        }
        None => claude_cmd,
    };

    let start_gate = if let Some(pane) = existing_pane.as_deref() {
        state
            .protocol
            .read()
            .await
            .sessions
            .get(name)
            .filter(|session| session.pane.as_deref() == Some(pane))
            .map(|session| (session.owner(), pane.to_string()))
    } else {
        None
    };
    let start_existing_pane = existing_pane.clone();
    let start_session_credential = session_start_credential.clone();
    let start_backend_cmd = claude_cmd.clone();
    let start_prompt = formatted_prompt.clone();
    let start_operation = move || {
        tokio::task::spawn_blocking({
            let window_name = window_name.clone();
            let tmux_session = tmux_session.clone();
            let existing_pane = start_existing_pane;
            let pane_credential = start_session_credential;
            let pane_incarnation = staged_incarnation;
            let direct_restart_lease_owner = direct_restart_lease_owner.clone();
            let backend_cmd = start_backend_cmd;
            let prompt = start_prompt;
            move || -> anyhow::Result<(String, bool)> {
                use std::process::Command;

                if cfg!(test) {
                    return Ok((format!("%test-restart-{window_name}"), true));
                }

                // Try respawn-pane on existing pane — kills the process and restarts
                // in-place, keeping the same pane ID and tmux session intact.
                //
                // For HttpApi backends the serve command is backgrounded (`&`), so
                // we respawn with a bare shell and then send-keys instead of letting
                // respawn-pane run the command directly (which would exit immediately).
                if let Some(ref pane) = existing_pane {
                    let mut prepared_command = prepare_backend_launch_command(
                        is_http_api,
                        &backend_cmd,
                        prompt.as_deref(),
                    )?;
                    let respawn_cmd = prepared_command.command().to_string();
                    // See `pane_env_args` docs for why OUIJA_SESSION_ID must
                    // be set on every pane spawn (including respawn-pane).
                    let env_args = crate::tmux::pane_env_args(
                        &window_name,
                        pane_credential.as_deref(),
                        pane_incarnation,
                    );
                    let mut respawn_args: Vec<&str> = vec!["respawn-pane", "-k"];
                    respawn_args.extend(env_args.iter().map(String::as_str));
                    respawn_args.extend_from_slice(&["-t", pane]);
                    let respawn_shell = crate::tmux::default_shell();
                    let direct_tui_command = (!is_http_api).then(|| {
                        format!(
                            "{} -lc {}",
                            crate::scheduler::shell_escape(&respawn_shell),
                            crate::scheduler::shell_escape(&respawn_cmd)
                        )
                    });
                    respawn_args.push(
                        direct_tui_command
                            .as_deref()
                            .unwrap_or(respawn_shell.as_str()),
                    );
                    crate::tmux::configure_managed_pane(pane);
                    let output = Command::new("tmux").args(&respawn_args).output();
                    match output {
                        Ok(o) if o.status.success() => {
                            if is_http_api {
                                // A backgrounded serve command would let a
                                // non-interactive shell exit immediately, so
                                // keep the fresh shell alive and launch it via
                                // terminal input.
                                std::thread::sleep(std::time::Duration::from_millis(300));
                                let hidden = format!(" {respawn_cmd}");
                                let status = Command::new("tmux")
                                    .args(["send-keys", "-t", pane, &hidden, "Enter"])
                                    .status()?;
                                if !status.success() {
                                    anyhow::bail!(
                                        "tmux send-keys failed for direct respawn pane {pane}"
                                    );
                                }
                            }
                            prepared_command.mark_handed_off();
                            tracing::info!("restart: respawn-pane {pane} succeeded");
                            return Ok((pane.clone(), false));
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
                    drop(prepared_command);

                    // A failed respawn can be ambiguous about whether tmux
                    // replaced the process before reporting failure. Remove the
                    // exact incumbent/staged managed pane before creating a
                    // fallback, otherwise overwriting the lease record would
                    // strand an unregistered backend.
                    if let Some(ref lease_owner) = direct_restart_lease_owner {
                        let live_owner = crate::tmux::inspect_pane_owner(pane)?;
                        let staged_owner = pane_incarnation.map(|incarnation| {
                            crate::daemon_protocol::ResourceOwner {
                                session_id: window_name.clone(),
                                incarnation,
                            }
                        });
                        let owned = live_owner.as_ref().is_some_and(|owner| {
                            crate::tmux::physical_owner_matches(owner, lease_owner)
                                || staged_owner.as_ref().is_some_and(|staged_owner| {
                                    crate::tmux::physical_owner_matches(owner, staged_owner)
                                })
                        });
                        if owned {
                            let status = Command::new("tmux")
                                .args(["kill-pane", "-t", pane])
                                .status()?;
                            let remaining_owner = crate::tmux::inspect_pane_owner(pane)?;
                            if !status.success()
                                && remaining_owner.as_ref().is_some_and(|owner| {
                                    crate::tmux::physical_owner_matches(owner, lease_owner)
                                        || staged_owner.as_ref().is_some_and(|staged_owner| {
                                            crate::tmux::physical_owner_matches(owner, staged_owner)
                                        })
                                })
                            {
                                anyhow::bail!(
                                    "failed to remove ambiguous direct respawn pane {pane}"
                                );
                            }
                        }
                    }
                }

                // Fallback: add window to existing tmux session, or create new one
                let tmux_session_exists = Command::new("tmux")
                    .args(["has-session", "-t", &tmux_session])
                    .output()
                    .is_ok_and(|o| o.status.success());

                let target = format!("{tmux_session}:");
                let env_args = crate::tmux::pane_env_args(
                    &window_name,
                    pane_credential.as_deref(),
                    pane_incarnation,
                );
                let output = if tmux_session_exists {
                    let mut args: Vec<&str> = vec!["new-window", "-d"];
                    args.extend(env_args.iter().map(String::as_str));
                    args.extend_from_slice(&[
                        "-t",
                        &target,
                        "-n",
                        &window_name,
                        "-P",
                        "-F",
                        "#{pane_id}",
                    ]);
                    Command::new("tmux").args(&args).output()?
                } else {
                    let mut args: Vec<&str> = vec!["new-session", "-d"];
                    args.extend(env_args.iter().map(String::as_str));
                    args.extend_from_slice(&[
                        "-s",
                        &tmux_session,
                        "-n",
                        &window_name,
                        "-P",
                        "-F",
                        "#{pane_id}",
                    ]);
                    Command::new("tmux").args(&args).output()?
                };
                if !output.status.success() {
                    anyhow::bail!(
                        "tmux session/window creation failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                let pane_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
                Ok((pane_id, true))
            }
        })
    };
    let start_result = if let Some((owner, pane)) = start_gate {
        state
            .with_owned_pane_claim(&owner, &pane, start_operation)
            .await
            .unwrap_or_else(|| {
                Ok(Err(anyhow::anyhow!(
                    "pane ownership changed before restart"
                )))
            })
    } else {
        start_operation().await
    };

    match start_result {
        Ok(Ok((pane_id, launch_after_registration))) => {
            if launch_after_registration {
                // A fallback window has a new inert pane. Publish that pane
                // durably under the exact staged/current incarnation before
                // any backend command can run. This path must not use ordinary
                // Register because `/start` existing-session restarts hold a
                // Restarting lease that intentionally rejects unowned writes.
                if let Some(expected_incarnation) = staged_incarnation {
                    let pane_owner = crate::daemon_protocol::ResourceOwner {
                        session_id: name.to_string(),
                        incarnation: expected_incarnation,
                    };
                    match state
                        .record_inert_start_pane(lease_owner, pane_owner.clone(), pane_id.clone())
                        .await
                    {
                        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
                        Ok(outcome) => {
                            let cleanup =
                                remove_inert_start_pane(state, &pane_owner, &pane_id).await;
                            return (
                                format!(
                                    "restart superseded before fallback pane registration ({outcome:?}){}",
                                    cleanup
                                        .err()
                                        .map(|error| format!(
                                            "; inert pane cleanup failed: {error}"
                                        ))
                                        .unwrap_or_default()
                                ),
                                None,
                                RestartOutcome::Superseded,
                            );
                        }
                        Err(error) => {
                            let cleanup =
                                remove_inert_start_pane(state, &pane_owner, &pane_id).await;
                            return (
                                format!(
                                    "restart failed to persist inert fallback pane: {error}{}",
                                    cleanup
                                        .err()
                                        .map(|cleanup_error| format!(
                                            "; inert pane cleanup failed: {cleanup_error}"
                                        ))
                                        .unwrap_or_default()
                                ),
                                None,
                                RestartOutcome::Failed,
                            );
                        }
                    }
                } else {
                    // Legacy restart of an absent session has no incumbent
                    // owner to compare. Chunk 8 moves that path onto a restart
                    // lease; keep its existing registration behavior here.
                    let metadata = crate::daemon_protocol::SessionMeta {
                        project_dir: Some(dir.clone()),
                        backend: Some(backend_name.clone()),
                        session_start_credential: session_start_credential.clone(),
                        ..Default::default()
                    };
                    state
                        .apply_and_execute(crate::daemon_protocol::Event::Register {
                            id: name.to_string(),
                            pane: Some(pane_id.clone()),
                            metadata,
                        })
                        .await;
                }

                let pane_for_launch = pane_id.clone();
                let mut prepared_command = match prepare_backend_launch_command(
                    is_http_api,
                    &claude_cmd,
                    formatted_prompt.as_deref(),
                ) {
                    Ok(command) => command,
                    Err(error) => {
                        if let Err(rollback_error) = rollback_claimed_restart(
                            state,
                            lease_owner,
                            &restart_target_owner,
                            Some(&pane_id),
                        )
                        .await
                        {
                            tracing::warn!(
                                "failed to durably roll back restart for {name}: {rollback_error}"
                            );
                        }
                        return (
                            format!("restart failed to prepare launch prompt: {error}"),
                            None,
                            RestartOutcome::Failed,
                        );
                    }
                };
                let command = if is_http_api {
                    prepared_command.command().to_string()
                } else {
                    crate::tmux::close_shell_after(prepared_command.command())
                };
                let launch_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    if cfg!(test) {
                        return Ok(());
                    }

                    crate::tmux::configure_managed_pane(&pane_for_launch);
                    let hidden_cmd = format!(" {command}");
                    let status = std::process::Command::new("tmux")
                        .args(["send-keys", "-t", &pane_for_launch, &hidden_cmd, "Enter"])
                        .status()?;
                    if !status.success() {
                        anyhow::bail!("tmux send-keys failed for pane {pane_for_launch}");
                    }
                    prepared_command.mark_handed_off();
                    Ok(())
                })
                .await;
                match launch_result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        if let Err(rollback_error) = rollback_claimed_restart(
                            state,
                            lease_owner,
                            &restart_target_owner,
                            Some(&pane_id),
                        )
                        .await
                        {
                            tracing::warn!(
                                "failed to durably roll back restart for {name}: {rollback_error}"
                            );
                        }
                        return (
                            format!("restart failed: {error}"),
                            None,
                            RestartOutcome::Failed,
                        );
                    }
                    Err(error) => {
                        if let Err(rollback_error) = rollback_claimed_restart(
                            state,
                            lease_owner,
                            &restart_target_owner,
                            Some(&pane_id),
                        )
                        .await
                        {
                            tracing::warn!(
                                "failed to durably roll back restart for {name}: {rollback_error}"
                            );
                        }
                        return (
                            format!("restart failed: {error}"),
                            None,
                            RestartOutcome::Failed,
                        );
                    }
                }
            }

            let mut reused_previous_backend_session = false;
            let backend_plan = select_http_restart_backend_plan(
                is_http_api,
                fresh,
                &backend_name,
                prev_metadata.as_ref(),
            );
            let mut backend_session_id = None;

            // A non-fresh HttpApi restart preserves history by trying the
            // stored session before creating a new one. Reuse deliberately
            // does not record a restart backend claim: this restart did not
            // create the session and therefore must not delete it on rollback.
            if let Some(HttpRestartBackendPlan::Reuse(prev_sid)) = backend_plan {
                let port = state.opencode_serve_port();
                let check_url = format!("http://127.0.0.1:{port}/session/{prev_sid}");
                match state
                    .http_client
                    .get(&check_url)
                    .timeout(std::time::Duration::from_secs(2))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        // Guard reuse against serve/attach-client version
                        // skew: a mismatched attach TUI crashes to a bare
                        // pane. Show the notice and keep the reused session
                        // registered API-only instead (mirrors the
                        // fresh-create guard in setup_shared_serve_session).
                        if let Some((serve_v, client_v)) =
                            opencode_attach_skew(&state.http_client, port).await
                        {
                            tracing::warn!(
                                port,
                                pane = %pane_id,
                                backend_session_id = %prev_sid,
                                serve_version = %serve_v,
                                attach_client_version = %client_v,
                                "opencode attach client/serve version skew on reuse; skipping attach TUI (would crash). Session remains functional via HTTP API."
                            );
                            notify_pane_opencode_attach_skew(&pane_id, &serve_v, &client_v, port)
                                .await;
                            backend_session_id = Some(prev_sid);
                            reused_previous_backend_session = true;
                        } else {
                            match launch_opencode_attach_for_session(
                                &pane_id, &dir, &prev_sid, port,
                            )
                            .await
                            .and_then(|attach_ready| {
                                previous_backend_session_after_attach(
                                    prev_sid.clone(),
                                    attach_ready,
                                    &pane_id,
                                )
                            }) {
                                Ok(sid) => {
                                    backend_session_id = Some(sid);
                                    reused_previous_backend_session = true;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "previous backend_session_id {prev_sid} is reachable but attach failed: {e}"
                                    );
                                }
                            }
                        }
                    }
                    _ => {
                        tracing::warn!(
                            "previous backend_session_id {prev_sid} is stale, creating new session"
                        );
                    }
                }
            }

            // Fresh restarts and failed/unavailable reuse create a new session
            // on the shared OpenCode serve.
            if is_http_api && backend_session_id.is_none() {
                let setup_result =
                    if restart_target_is_current(state, lease_owner, &restart_target_owner).await {
                        setup_shared_serve_session(
                            state,
                            &restart_target_owner,
                            Some(lease_owner),
                            &pane_id,
                            &dir,
                        )
                        .await
                    } else {
                        Err(anyhow::anyhow!(
                            "restart owner changed before OpenCode session setup"
                        ))
                    };
                backend_session_id = match setup_result {
                    Ok(sid) => Some(sid),
                    Err(e) => {
                        tracing::warn!("shared serve session setup failed: {e}");
                        if restart_backend_cleanup_pending(
                            state,
                            lease_owner,
                            &restart_target_owner,
                        )
                        .await
                        {
                            return (
                                format!(
                                    "restart failed for '{name}' with durable backend cleanup pending"
                                ),
                                None,
                                RestartOutcome::Failed,
                            );
                        }
                        None
                    }
                };
            }

            if is_http_api && backend_session_id.is_none() {
                tracing::warn!(
                    "restart_session: not registering {name} because OpenCode attach setup failed"
                );
                if let Err(rollback_error) = rollback_claimed_restart(
                    state,
                    lease_owner,
                    &restart_target_owner,
                    Some(&pane_id),
                )
                .await
                {
                    tracing::warn!(
                        "failed to durably roll back restart for {name}: {rollback_error}"
                    );
                }
                return (
                    format!(
                        "restart failed: OpenCode attach setup failed for '{name}' (pane {pane_id})"
                    ),
                    None,
                    RestartOutcome::Failed,
                );
            }

            // Codex may have already consumed the credential and recorded its
            // thread ID while the pane was starting. Preserve that atomic
            // result instead of overwriting it with this restart's initial
            // `None` placeholder during the metadata refresh below.
            let session_start_result = if session_start_credential.is_some() {
                let proto = state.protocol.read().await;
                proto.sessions.get(name).map(|session| {
                    (
                        session.metadata.backend_session_id.clone(),
                        session.metadata.session_start_credential.clone(),
                    )
                })
            } else {
                None
            };
            let (backend_session_id, pending_session_start_credential) =
                final_restart_backend_binding(
                    &backend_name,
                    resume_id.clone(),
                    session_start_credential.clone(),
                    backend_session_id,
                    session_start_result,
                );
            let restart_backend_session_id = backend_session_id.clone();

            let opencode_binding = opencode_binding_for_restart_session(
                is_http_api,
                backend_session_id.as_deref(),
                reused_previous_backend_session,
                prev_metadata
                    .as_ref()
                    .and_then(|m| m.opencode_binding.clone()),
            );
            let restart_opencode_binding = opencode_binding.clone();
            let proto_meta = match prev_metadata {
                Some(ref m) => crate::daemon_protocol::SessionMeta {
                    project_dir: Some(dir.clone()),
                    role: m.role.clone(),
                    bulletin: m.bulletin.clone(),
                    networked: m.networked,
                    worktree: m.worktree,
                    vim_mode: m.vim_mode,
                    backend_session_id,
                    backend: Some(backend_name.clone()),
                    session_start_credential: pending_session_start_credential.clone(),
                    backend_repair_reservation: m.backend_repair_reservation.clone(),
                    opencode_binding: opencode_binding.clone(),
                    restart_generation: m.restart_generation.saturating_add(1),
                    session_incarnation: m.session_incarnation,
                    project_description: m.project_description.clone(),
                    last_metadata_update: None,
                    model: effective_model.clone(),
                    effort: effective_effort.clone(),
                    codex_home: launch_codex_home.clone(),
                    reminder: effective_manual_reminder.clone(),
                    parent_session: effective_parent_session.clone(),
                    idle_policy: effective_idle_policy.clone(),
                    prompt: resolved_prompt.stored.clone(),
                    iteration: m.iteration,
                    iteration_log: m.iteration_log.clone(),
                    last_iteration_at: m.last_iteration_at,
                    on_fire: m.on_fire.clone(),
                    // Session is being freshly re-registered with a known
                    // project_dir; let the next worktree sweep populate
                    // presence rather than carrying a stale bit across
                    // restart (the dir may have been recreated out of band).
                    worktree_present: None,
                    // The finalizer must preserve the live target's durable
                    // accounting. Exact fresh success only finalizes its
                    // provisional reset; it must not zero target work again.
                    fresh_context_after_active_secs: active_context_policy_for_launch(
                        m.fresh_context_after_active_secs,
                        fresh_context_after_active_secs,
                        fresh,
                    ),
                    active_context_accumulated_secs: m.active_context_accumulated_secs,
                    active_context_segment_started_at: m.active_context_segment_started_at,
                    active_context_restart_due: m.active_context_restart_due,
                    active_context_accounting_provisional: m.active_context_accounting_provisional,
                },
                None => crate::daemon_protocol::SessionMeta {
                    project_dir: Some(dir.clone()),
                    backend: Some(backend_name.clone()),
                    backend_session_id,
                    session_start_credential: pending_session_start_credential,
                    opencode_binding,
                    model: effective_model.clone(),
                    effort: effective_effort.clone(),
                    codex_home: launch_codex_home.clone(),
                    reminder: effective_manual_reminder.clone(),
                    parent_session: effective_parent_session.clone(),
                    idle_policy: effective_idle_policy.clone(),
                    prompt: resolved_prompt.stored.clone(),
                    fresh_context_after_active_secs: active_context_policy_for_launch(
                        None,
                        fresh_context_after_active_secs,
                        fresh,
                    ),
                    ..Default::default()
                },
            };
            #[cfg(test)]
            state
                .wait_restart_test_checkpoint(
                    crate::state::RestartTestCheckpoint::HardBeforeCompletion,
                )
                .await;
            match state
                .complete_requested_restart_launch(
                    lease_owner,
                    &restart_target_owner,
                    Some(pane_id.clone()),
                    proto_meta,
                    true,
                    fresh,
                )
                .await
            {
                Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
                Ok(_) => {
                    return (
                        format!("restart superseded before final metadata for '{name}'"),
                        None,
                        RestartOutcome::Superseded,
                    );
                }
                Err(error) => {
                    return (
                        format!("restart failed to persist final metadata for '{name}': {error}"),
                        None,
                        RestartOutcome::Failed,
                    );
                }
            }
            // Strong HttpApi bindings can use readiness delivery; weak reused
            // panes must stay on raw tmux to preserve the visible-pane boundary.
            if should_schedule_restart_prompt_injection(
                is_http_api,
                restart_backend_session_id.as_deref(),
                restart_opencode_binding.as_ref(),
            ) {
                if let Some(ref prompt_text) = formatted_prompt {
                    schedule_prompt_injection_for_owner(
                        state,
                        name,
                        pane_id.clone(),
                        prompt_text.clone(),
                        restart_backend_session_id.clone(),
                        Some(restart_target_owner.clone()),
                    );
                }
            } else if is_http_api && restart_backend_session_id.is_some() {
                if let Some(ref prompt_text) = formatted_prompt {
                    if let Err(e) = deliver_prompt_fallback(
                        state,
                        name,
                        &pane_id,
                        prompt_text,
                        true,
                        false,
                        restart_backend_session_id.as_deref(),
                        Some(&restart_target_owner),
                    )
                    .await
                    {
                        tracing::warn!("restart prompt fallback delivery failed for {name}: {e}");
                        restore_restart_prompt_after_fallback_failure(
                            state,
                            name,
                            crate::state::PendingPrompt::new(
                                pane_id.clone(),
                                prompt_text.clone(),
                                restart_backend_session_id.clone(),
                            )
                            .with_owner(restart_target_owner.clone()),
                        );
                    }
                }
            }
            (
                format!("restarted '{name}' in {dir} (pane {pane_id})"),
                prompt_msg_id,
                RestartOutcome::Restarted,
            )
        }
        Ok(Err(e)) => {
            if let Err(rollback_error) = rollback_claimed_restart(
                state,
                lease_owner,
                &restart_target_owner,
                existing_pane.as_deref(),
            )
            .await
            {
                tracing::warn!("failed to durably roll back restart for {name}: {rollback_error}");
            }
            (format!("restart failed: {e}"), None, RestartOutcome::Failed)
        }
        Err(e) => {
            if let Err(rollback_error) = rollback_claimed_restart(
                state,
                lease_owner,
                &restart_target_owner,
                existing_pane.as_deref(),
            )
            .await
            {
                tracing::warn!("failed to durably roll back restart for {name}: {rollback_error}");
            }
            (format!("restart failed: {e}"), None, RestartOutcome::Failed)
        }
    }
}

async fn restart_target_is_current(
    state: &AppState,
    lease_owner: &crate::daemon_protocol::ResourceOwner,
    target_owner: &crate::daemon_protocol::ResourceOwner,
) -> bool {
    let proto = state.protocol.read().await;
    proto
        .lifecycle_leases
        .get(&lease_owner.session_id)
        .is_some_and(|lease| {
            lease.owner == *lease_owner
                && lease.phase == crate::daemon_protocol::LifecyclePhase::Restarting
                && lease.restart_target_owner.as_ref() == Some(target_owner)
        })
        && proto
            .sessions
            .get(&target_owner.session_id)
            .is_some_and(|session| session.owner() == *target_owner)
}

async fn restart_backend_cleanup_pending(
    state: &AppState,
    lease_owner: &crate::daemon_protocol::ResourceOwner,
    target_owner: &crate::daemon_protocol::ResourceOwner,
) -> bool {
    state
        .protocol
        .read()
        .await
        .lifecycle_leases
        .get(&lease_owner.session_id)
        .is_some_and(|lease| {
            lease.owner == *lease_owner
                && lease.phase == crate::daemon_protocol::LifecyclePhase::Restarting
                && lease.restart_target_owner.as_ref() == Some(target_owner)
                && lease.backend_session_owner.as_ref() == Some(target_owner)
                && lease.backend_session_id.is_some()
        })
}

async fn delete_claimed_restart_backend(
    state: &std::sync::Arc<AppState>,
    lease_owner: &crate::daemon_protocol::ResourceOwner,
    target_owner: &crate::daemon_protocol::ResourceOwner,
    port: u16,
    backend_session_id: &str,
    context: &str,
) -> bool {
    if !restart_target_is_current(state, lease_owner, target_owner).await {
        return false;
    }
    let deleted = delete_owned_opencode_session(
        state,
        target_owner.clone(),
        port,
        backend_session_id,
        context,
    )
    .await;
    if !deleted {
        return false;
    }
    matches!(
        state
            .clear_restart_backend_claim(lease_owner, target_owner, backend_session_id)
            .await,
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied)
    )
}

/// Soft restart for HttpApi backends: create a new session on the opencode serve
/// via HTTP API and deliver the prompt directly. Then respawn the TUI attach to
/// point at the new session so the human can interact.
///
/// `model` and `effort` are applied to the delivered prompt_async body via
/// [`opencode_prompt_body`] so the new session runs with the right model /
/// variant from the first request.
///
/// Returns `Ok((status_message, prompt_msg_id))` on success.
/// Returns `Err(())` on failure — caller should fall back to hard restart.
#[allow(clippy::too_many_arguments)]
async fn soft_restart_session_claimed(
    state: &std::sync::Arc<AppState>,
    lease_owner: &crate::daemon_protocol::ResourceOwner,
    target_owner: &crate::daemon_protocol::ResourceOwner,
    previous_metadata: &crate::daemon_protocol::SessionMeta,
    name: &str,
    pane: Option<&str>,
    project_dir: &str,
    prompt: Option<&str>,
    prompt_replacement: Option<&str>,
    fresh_context_after_active_secs: Option<u64>,
    from: Option<&str>,
    expects_reply: Option<bool>,
    reminder: Option<&str>,
    parent_session_override: ParentSessionOverride,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(String, Option<u64>), ()> {
    let port = state.opencode_serve_port();
    if !restart_target_is_current(state, lease_owner, target_owner).await {
        tracing::warn!("soft restart: target authority disappeared before backend creation");
        return Err(());
    }
    if let Some(pane) = pane {
        match state
            .record_inert_start_pane(lease_owner, target_owner.clone(), pane.to_string())
            .await
        {
            Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
            Ok(outcome) => {
                tracing::warn!(
                    "soft restart: target pane authority was superseded before backend creation ({outcome:?})"
                );
                return Err(());
            }
            Err(error) => {
                tracing::warn!(
                    "soft restart: failed to persist target pane authority before backend creation: {error}"
                );
                return Err(());
            }
        }
    }
    // 1. Create a new session on the opencode serve
    let resp = state
        .http_client
        .post(format!("http://127.0.0.1:{port}/session"))
        .header("x-opencode-directory", project_dir)
        .json(&serde_json::json!({}))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;
    let new_session_id = match resp {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.map_err(|e| {
                tracing::warn!("soft restart: failed to parse session response: {e}");
            })?;
            body["id"]
                .as_str()
                .map(String::from)
                .ok_or_else(|| {
                    tracing::warn!("soft restart: no session id in opencode response");
                })
                .and_then(|session_id| {
                    validate_created_opencode_session_id(&session_id).map_err(|error| {
                        tracing::warn!("soft restart: {error}");
                    })
                })?
        }
        Ok(r) => {
            let status = r.status();
            tracing::warn!("soft restart: POST /session failed with {status}");
            return Err(());
        }
        Err(e) => {
            tracing::warn!("soft restart: POST /session request failed: {e}");
            return Err(());
        }
    };
    match state
        .record_restart_backend_claim(
            lease_owner,
            target_owner,
            "opencode".into(),
            new_session_id.clone(),
        )
        .await
    {
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
        Ok(outcome) => {
            tracing::warn!(
                "soft restart: target backend claim was superseded after creation ({outcome:?})"
            );
            delete_owned_opencode_session(
                state,
                target_owner.clone(),
                port,
                &new_session_id,
                "soft restart unclaimed backend cleanup",
            )
            .await;
            return Err(());
        }
        Err(error) => {
            tracing::warn!(
                "soft restart: failed to persist target backend claim after creation: {error}"
            );
            delete_owned_opencode_session(
                state,
                target_owner.clone(),
                port,
                &new_session_id,
                "soft restart unpersisted backend cleanup",
            )
            .await;
            return Err(());
        }
    }
    #[cfg(test)]
    state
        .wait_restart_test_checkpoint(crate::state::RestartTestCheckpoint::SoftAfterBackendClaim)
        .await;

    tracing::info!(
        "soft restart: created new opencode session {new_session_id} for '{name}' (port {port})"
    );

    // 2. Snapshot metadata before attach/prompt delivery. Metadata for the
    //    replacement backend is committed only after the delivery boundary that
    //    makes the restart safe: attach verification for pane-backed sessions,
    //    and prompt_async acceptance for headless sessions.
    //
    //    When `model` / `effort` are None we preserve the session's current
    //    metadata rather than clearing it: callers are expected to pre-compute
    //    the effective values (restart_session does this via prev_metadata
    //    fallback), but a stale snapshot or a future caller that forgets the
    //    fallback must not silently wipe fields that were set by another
    //    writer between the snapshot and this atomic block.
    if !restart_target_is_current(state, lease_owner, target_owner).await {
        delete_claimed_restart_backend(
            state,
            lease_owner,
            target_owner,
            port,
            &new_session_id,
            "soft restart stale-target cleanup",
        )
        .await;
        return Err(());
    }
    let owner_snapshot = SoftRestartOwnerSnapshot {
        session_id: target_owner.session_id.clone(),
        incarnation: target_owner.incarnation,
    };
    let restart_generation = state
        .protocol
        .read()
        .await
        .sessions
        .get(name)
        .map(|session| session.metadata.restart_generation)
        .ok_or(())?;
    let effective_parent_session = parent_session_override.resolve(Some(previous_metadata));
    let reminder_meta = crate::daemon_protocol::SessionMeta {
        reminder: reminder.map(String::from),
        parent_session: effective_parent_session.clone(),
        idle_policy: idle_policy.clone(),
        ..Default::default()
    };
    let effective_reminder = reminder_meta.effective_reminder(name, None);

    let mut prompt_msg_id = None;
    let mut metadata_committed = false;

    // 3. Respawn the TUI attach to point at the new session.
    if let Some(pane) = pane {
        match respawn_opencode_attach_for_session(
            state,
            &owner_snapshot.resource_owner(),
            target_owner,
            pane,
            project_dir,
            &new_session_id,
            port,
            name,
        )
        .await
        {
            Ok(true) => {
                if should_commit_soft_restart_metadata_before_prompt(Some(pane), prompt)
                    && complete_soft_restart_metadata(
                        state,
                        lease_owner,
                        target_owner,
                        Some(pane),
                        &owner_snapshot,
                        &new_session_id,
                        restart_generation,
                        SoftRestartMetadataUpdate {
                            prompt_replacement,
                            fresh_context_after_active_secs,
                            reminder,
                            parent_session: parent_session_override.clone(),
                            idle_policy: idle_policy.clone(),
                            model,
                            effort,
                        },
                    )
                    .await
                    .is_err()
                {
                    rollback_pane_after_failed_soft_restart_commit(
                        state,
                        &owner_snapshot,
                        pane,
                        project_dir,
                        port,
                        name,
                        previous_metadata,
                    )
                    .await;
                    delete_claimed_restart_backend(
                        state,
                        lease_owner,
                        target_owner,
                        port,
                        &new_session_id,
                        "soft restart cleanup",
                    )
                    .await;
                    return Err(());
                }
                if should_commit_soft_restart_metadata_before_prompt(Some(pane), prompt) {
                    metadata_committed = true;
                }
            }
            Ok(false) => {
                tracing::warn!("soft restart: opencode attach did not start in pane {pane}");
                delete_claimed_restart_backend(
                    state,
                    lease_owner,
                    target_owner,
                    port,
                    &new_session_id,
                    "soft restart cleanup",
                )
                .await;
                return Err(());
            }
            Err(e) => {
                tracing::warn!("soft restart: respawn-pane {pane} failed: {e}");
                delete_claimed_restart_backend(
                    state,
                    lease_owner,
                    target_owner,
                    port,
                    &new_session_id,
                    "soft restart cleanup",
                )
                .await;
                return Err(());
            }
        }
    }

    // 4. Deliver prompt directly via HTTP API after any required attach
    //    succeeded. This preserves the Err boundary: attach failure returns
    //    before prompt_async can start work in the throwaway session.
    if let Some(text) = prompt {
        if !restart_target_is_current(state, lease_owner, target_owner).await {
            delete_claimed_restart_backend(
                state,
                lease_owner,
                target_owner,
                port,
                &new_session_id,
                "soft restart stale-target cleanup",
            )
            .await;
            return Err(());
        }

        let full_text = match effective_reminder.as_deref() {
            Some(r) => format!("{text}\n\n{r}"),
            None => text.to_string(),
        };
        let message = if let Some(sender) = from {
            let er = expects_reply.unwrap_or(true);
            let msg_id = {
                let mut proto = state.protocol.write().await;
                proto.next_seq()
            };
            prompt_msg_id = Some(msg_id);
            crate::daemon_protocol::format_session_message(
                sender, &full_text, er, msg_id, None, false,
            )
        } else {
            full_text
        };

        match deliver_soft_restart_prompt(
            state,
            port,
            &new_session_id,
            project_dir,
            &message,
            model,
            effort,
        )
        .await
        {
            crate::state::DeliveryOutcome::Accepted => {}
            crate::state::DeliveryOutcome::Ambiguous(reason) => {
                tracing::warn!(
                    "soft restart: prompt_async outcome ambiguous for {new_session_id}: {reason}; retaining the target to avoid duplicate work"
                );
            }
            crate::state::DeliveryOutcome::Rejected(reason) => {
                tracing::warn!("soft restart: prompt_async failed for {new_session_id}: {reason}");
                if let (Some(pane), Some(previous_session_id)) = (
                    pane,
                    previous_backend_session_for_prompt_failure_rollback(pane, previous_metadata),
                ) {
                    match respawn_opencode_attach_for_session(
                        state,
                        &owner_snapshot.resource_owner(),
                        lease_owner,
                        pane,
                        project_dir,
                        previous_session_id,
                        port,
                        name,
                    )
                    .await
                    {
                        Ok(true) => {}
                        Ok(false) => tracing::warn!(
                            "soft restart: failed to reattach pane {pane} to previous opencode session after prompt_async failure"
                        ),
                        Err(error) => tracing::warn!(
                            "soft restart: failed to roll back pane {pane} to previous opencode session after prompt_async failure: {error}"
                        ),
                    }
                }
                delete_claimed_restart_backend(
                    state,
                    lease_owner,
                    target_owner,
                    port,
                    &new_session_id,
                    "soft restart cleanup",
                )
                .await;
                return Err(());
            }
        }
    }

    if !metadata_committed
        && complete_soft_restart_metadata(
            state,
            lease_owner,
            target_owner,
            pane,
            &owner_snapshot,
            &new_session_id,
            restart_generation,
            SoftRestartMetadataUpdate {
                prompt_replacement,
                fresh_context_after_active_secs,
                reminder,
                parent_session: parent_session_override,
                idle_policy,
                model,
                effort,
            },
        )
        .await
        .is_err()
    {
        delete_claimed_restart_backend(
            state,
            lease_owner,
            target_owner,
            port,
            &new_session_id,
            "soft restart cleanup",
        )
        .await;
        return Err(());
    }

    Ok((
        format!("soft-restarted '{name}' in {project_dir} (session {new_session_id})"),
        prompt_msg_id,
    ))
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn soft_restart_session(
    state: &std::sync::Arc<AppState>,
    name: &str,
    pane: Option<&str>,
    project_dir: &str,
    prompt: Option<&str>,
    from: Option<&str>,
    expects_reply: Option<bool>,
    reminder: Option<&str>,
    parent_session_override: ParentSessionOverride,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(String, Option<u64>), ()> {
    let (lease_owner, previous_metadata) = {
        let proto = state.protocol.read().await;
        let session = proto.sessions.get(name).ok_or(())?;
        (session.owner(), session.metadata.clone())
    };
    if state
        .claim_existing_start(&lease_owner)
        .await
        .map_err(|_| ())?
        != crate::daemon_protocol::LifecycleMutationOutcome::Applied
    {
        return Err(());
    }
    let target_owner = match state
        .stage_restart_launch(
            &lease_owner,
            "opencode".to_string(),
            true,
            true,
            None,
            None,
            None,
        )
        .await
    {
        crate::daemon_protocol::StageFreshLaunchOutcome::Staged { incarnation } => {
            crate::daemon_protocol::ResourceOwner {
                session_id: name.to_string(),
                incarnation,
            }
        }
        _ => {
            let _ = state.abort_lifecycle(&lease_owner).await;
            return Err(());
        }
    };
    let result = soft_restart_session_claimed(
        state,
        &lease_owner,
        &target_owner,
        &previous_metadata,
        name,
        pane,
        project_dir,
        prompt,
        None,
        None,
        from,
        expects_reply,
        reminder,
        parent_session_override,
        idle_policy,
        model,
        effort,
    )
    .await;
    if result.is_err() {
        let _ = state
            .rollback_restart_launch(&lease_owner, &target_owner, None)
            .await;
    }
    result
}

fn should_commit_soft_restart_metadata_before_prompt(
    _pane: Option<&str>,
    prompt: Option<&str>,
) -> bool {
    prompt.is_none()
}

#[allow(clippy::too_many_arguments)]
async fn complete_soft_restart_metadata(
    state: &std::sync::Arc<AppState>,
    lease_owner: &crate::daemon_protocol::ResourceOwner,
    target_owner: &crate::daemon_protocol::ResourceOwner,
    pane: Option<&str>,
    owner: &SoftRestartOwnerSnapshot,
    new_session_id: &str,
    expected_restart_generation: u64,
    update: SoftRestartMetadataUpdate<'_>,
) -> Result<(), ()> {
    if owner.resource_owner() != *target_owner {
        return Err(());
    }
    let mut metadata = {
        let proto = state.protocol.read().await;
        let lease_matches = proto
            .lifecycle_leases
            .get(&lease_owner.session_id)
            .is_some_and(|lease| {
                lease.owner == *lease_owner
                    && lease.phase == crate::daemon_protocol::LifecyclePhase::Restarting
                    && lease.restart_target_owner.as_ref() == Some(target_owner)
            });
        let Some(session) = proto.sessions.get(&target_owner.session_id) else {
            return Err(());
        };
        if !lease_matches
            || session.owner() != *target_owner
            || session.metadata.restart_generation != expected_restart_generation
        {
            return Err(());
        }
        session.metadata.clone()
    };
    metadata.backend = Some("opencode".to_string());
    metadata.backend_session_id = Some(new_session_id.to_string());
    metadata.opencode_binding = Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged);
    if metadata
        .backend_repair_reservation
        .as_ref()
        .is_some_and(|reservation| {
            reservation.restart_generation == metadata.restart_generation
                && reservation.phase == crate::daemon_protocol::BackendRepairPhase::Staged
        })
    {
        metadata.backend_repair_reservation = None;
    }
    if let Some(reminder) = update.reminder {
        metadata.reminder = Some(reminder.to_string());
    }
    if let Some(prompt) = update.prompt_replacement {
        metadata.prompt = Some(prompt.to_string());
    }
    if let Some(limit) = update.fresh_context_after_active_secs {
        metadata.fresh_context_after_active_secs = Some(limit);
    }
    match update.parent_session {
        ParentSessionOverride::PreservePrevious => {}
        ParentSessionOverride::SetParent(parent) => {
            metadata.parent_session = Some(parent);
        }
        ParentSessionOverride::NoParent => {
            metadata.parent_session = None;
        }
    }
    if let Some(policy) = update.idle_policy {
        metadata.idle_policy = Some(policy);
    }
    if let Some(model) = update.model {
        metadata.model = Some(model.to_string());
    }
    if let Some(effort) = update.effort {
        metadata.effort = Some(effort.to_string());
    }
    match state
        .complete_requested_restart_launch(
            lease_owner,
            target_owner,
            pane.map(str::to_owned),
            metadata,
            false,
            true,
        )
        .await
    {
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => Ok(()),
        Ok(outcome) => {
            tracing::warn!(
                session = %target_owner.session_id,
                incarnation = %target_owner.incarnation,
                "soft restart final metadata was superseded: {outcome:?}"
            );
            Err(())
        }
        Err(error) => {
            tracing::warn!(
                session = %target_owner.session_id,
                incarnation = %target_owner.incarnation,
                "failed to persist soft-restart final metadata: {error}"
            );
            Err(())
        }
    }
}

#[cfg(test)]
async fn apply_soft_restart_metadata(
    state: &AppState,
    owner: &SoftRestartOwnerSnapshot,
    new_session_id: &str,
    expected_restart_generation: u64,
    update: SoftRestartMetadataUpdate<'_>,
) -> Result<(), ()> {
    state
        .with_backend_binding_transition(&owner.session_id, Some(new_session_id), |proto| {
            let Some(session) = proto.sessions.get_mut(&owner.session_id) else {
                return Err(());
            };
            if session.metadata.session_incarnation != owner.incarnation {
                return Err(());
            }
            if session.metadata.restart_generation != expected_restart_generation {
                return Err(());
            }
            session.metadata.backend = Some("opencode".to_string());
            session.metadata.backend_session_id = Some(new_session_id.to_string());
            session.metadata.opencode_binding =
                Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged);
            session.metadata.restart_generation =
                session.metadata.restart_generation.saturating_add(1);
            if session
                .metadata
                .backend_repair_reservation
                .as_ref()
                .is_some_and(|reservation| {
                    reservation.restart_generation == session.metadata.restart_generation
                        && reservation.phase == crate::daemon_protocol::BackendRepairPhase::Staged
                })
            {
                session.metadata.backend_repair_reservation = None;
            }
            if let Some(r) = update.reminder {
                session.metadata.reminder = Some(r.to_string());
            }
            if let Some(prompt) = update.prompt_replacement {
                session.metadata.prompt = Some(prompt.to_string());
            }
            if let Some(limit) = update.fresh_context_after_active_secs {
                session.metadata.fresh_context_after_active_secs = Some(limit);
            }
            match update.parent_session {
                ParentSessionOverride::PreservePrevious => {}
                ParentSessionOverride::SetParent(parent) => {
                    session.metadata.parent_session = Some(parent);
                }
                ParentSessionOverride::NoParent => {
                    session.metadata.parent_session = None;
                }
            }
            if let Some(policy) = update.idle_policy {
                session.metadata.idle_policy = Some(policy);
            }
            if let Some(m) = update.model {
                session.metadata.model = Some(m.to_string());
            }
            if let Some(e) = update.effort {
                session.metadata.effort = Some(e.to_string());
            }
            if let Err(error) = state.persist_protocol_state(proto) {
                tracing::warn!("failed to persist soft-restart metadata: {error}");
            }
            Ok(())
        })
        .await
}

#[derive(Default)]
struct SoftRestartMetadataUpdate<'a> {
    prompt_replacement: Option<&'a str>,
    fresh_context_after_active_secs: Option<u64>,
    reminder: Option<&'a str>,
    parent_session: ParentSessionOverride,
    idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
    model: Option<&'a str>,
    effort: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SoftRestartOwnerSnapshot {
    session_id: String,
    incarnation: crate::daemon_protocol::SessionIncarnation,
}

impl SoftRestartOwnerSnapshot {
    fn resource_owner(&self) -> crate::daemon_protocol::ResourceOwner {
        crate::daemon_protocol::ResourceOwner {
            session_id: self.session_id.clone(),
            incarnation: self.incarnation,
        }
    }
}

#[cfg(test)]
async fn restore_soft_restart_metadata(
    state: &AppState,
    name: &str,
    failed_session_id: &str,
    previous_metadata: &crate::daemon_protocol::SessionMeta,
) {
    state
        .with_backend_binding_transition(
            name,
            previous_metadata.backend_session_id.as_deref(),
            |proto| {
                let Some(session) = proto.sessions.get_mut(name) else {
                    return;
                };
                if session.metadata.backend_session_id.as_deref() != Some(failed_session_id) {
                    return;
                }
                session.metadata.backend = previous_metadata.backend.clone();
                session.metadata.backend_session_id = previous_metadata.backend_session_id.clone();
                session.metadata.opencode_binding = previous_metadata.opencode_binding.clone();
                session.metadata.model = previous_metadata.model.clone();
                session.metadata.effort = previous_metadata.effort.clone();
                session.metadata.restart_generation = previous_metadata.restart_generation;
                if let Err(error) = state.persist_protocol_state(proto) {
                    tracing::warn!("failed to persist soft-restart rollback metadata: {error}");
                }
            },
        )
        .await;
}

async fn failed_soft_restart_commit_rollback_target(
    state: &AppState,
    name: &str,
    previous_metadata: &crate::daemon_protocol::SessionMeta,
) -> Option<String> {
    if let Some(previous_session_id) = previous_metadata.backend_session_id.clone() {
        return Some(previous_session_id);
    }

    let proto = state.protocol.read().await;
    proto
        .sessions
        .get(name)
        .and_then(|session| session.metadata.backend_session_id.clone())
}

async fn rollback_pane_after_failed_soft_restart_commit(
    state: &AppState,
    owner: &SoftRestartOwnerSnapshot,
    pane: &str,
    project_dir: &str,
    port: u16,
    name: &str,
    previous_metadata: &crate::daemon_protocol::SessionMeta,
) {
    let Some(target_session_id) =
        failed_soft_restart_commit_rollback_target(state, name, previous_metadata).await
    else {
        tracing::warn!(
            session = %name,
            pane,
            "soft restart: metadata commit failed after attach with no rollback backend session"
        );
        return;
    };

    match respawn_opencode_attach_for_session(
        state,
        &owner.resource_owner(),
        &owner.resource_owner(),
        pane,
        project_dir,
        &target_session_id,
        port,
        name,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            session = %name,
            pane,
            backend_session_id = %target_session_id,
            "soft restart: failed to roll pane back after stale metadata commit"
        ),
        Err(error) => tracing::warn!(
            session = %name,
            pane,
            backend_session_id = %target_session_id,
            "soft restart: failed to respawn rollback attach after stale metadata commit: {error}"
        ),
    }
}

fn previous_backend_session_for_prompt_failure_rollback<'a>(
    pane: Option<&str>,
    previous_metadata: &'a crate::daemon_protocol::SessionMeta,
) -> Option<&'a str> {
    pane?;
    previous_metadata.backend_session_id.as_deref()
}

async fn deliver_soft_restart_prompt(
    state: &AppState,
    port: u16,
    session_id: &str,
    project_dir: &str,
    message: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> crate::state::DeliveryOutcome {
    let body = opencode_prompt_body(message, model, effort);
    let async_url = format!("http://127.0.0.1:{port}/session/{session_id}/prompt_async");
    let resp = state
        .http_client
        .post(&async_url)
        .header("x-opencode-directory", project_dir)
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;
    match resp {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            let status = resp.status();
            let decision = classify_prompt_async_fallback(PromptAsyncFailure::Status(status));
            if decision.should_try_raw_tmux() {
                return crate::state::DeliveryOutcome::Rejected(format!(
                    "prompt_async returned {status}"
                ));
            }
            tracing::warn!(
                "soft restart: prompt_async status {status} is ambiguous; not retrying restart prompt"
            );
            return crate::state::DeliveryOutcome::Ambiguous(format!(
                "prompt_async returned {status}"
            ));
        }
        Err(error) => {
            let decision = classify_prompt_async_fallback(PromptAsyncFailure::Request(&error));
            if decision.should_try_raw_tmux() {
                return crate::state::DeliveryOutcome::Rejected(format!(
                    "prompt_async request failed: {error}"
                ));
            }
            tracing::warn!(
                "soft restart: prompt_async request failure is ambiguous; not retrying restart prompt: {error}"
            );
            return crate::state::DeliveryOutcome::Ambiguous(format!(
                "prompt_async request failed: {error}"
            ));
        }
    }
    tracing::info!("soft restart: delivered prompt to {session_id} via prompt_async");
    crate::state::DeliveryOutcome::Accepted
}

/// Health-check the externally running opencode serve, create a session on it,
/// and launch `opencode attach` in the tmux pane.
///
/// Returns the opencode session ID on success.
fn shared_serve_session_after_attach(
    session_id: String,
    attach_ready: bool,
    pane_id: &str,
) -> anyhow::Result<String> {
    if attach_ready {
        Ok(session_id)
    } else {
        anyhow::bail!("opencode attach did not start in pane {pane_id}")
    }
}

fn validate_created_opencode_session_id(session_id: &str) -> anyhow::Result<String> {
    if let Some(error) = crate::daemon_protocol::validate_backend_session_id_boundary(session_id) {
        anyhow::bail!("{error}: {session_id:?}");
    }
    Ok(session_id.to_string())
}

fn previous_backend_session_after_attach(
    session_id: String,
    attach_ready: bool,
    pane_id: &str,
) -> anyhow::Result<String> {
    if attach_ready {
        Ok(session_id)
    } else {
        anyhow::bail!("previous opencode attach did not start in pane {pane_id}")
    }
}

fn wait_for_opencode_attach(pane_id: &str, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if crate::tmux::pane_alive(pane_id, &["opencode"]) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn opencode_attach_command(port: u16, session_id: &str, project_dir: &str) -> String {
    let escaped_session_id = crate::scheduler::shell_escape(session_id);
    let escaped_dir = crate::scheduler::shell_escape(project_dir);
    format!(
        "opencode attach http://127.0.0.1:{port} --session {escaped_session_id} --dir {escaped_dir}"
    )
}

/// Whether the local `opencode attach` client can drive the running serve.
///
/// The attach TUI is a Bun-compiled binary that decodes serve responses by
/// their exact shape. A mismatched client crashes on launch (observed:
/// `E?.data?.findLast is not a function`), dropping the tmux pane back to a
/// bare shell. We treat any non-empty version difference as incompatible —
/// the attach protocol carries no compatibility range, so equality is the
/// only safe predicate.
fn opencode_attach_versions_compatible(serve_version: &str, client_version: &str) -> bool {
    serve_version.trim() == client_version.trim()
}

/// Read the `opencode` client version via `opencode --version`.
///
/// Returns the trimmed first line of stdout, or `None` when the binary is
/// missing, exits non-zero, or prints nothing. A `None` result disables the
/// skew guard (fail open: never block a spawn just because the version could
/// not be read).
fn opencode_client_version() -> Option<String> {
    let output = std::process::Command::new("opencode")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = stdout.lines().next()?.trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// Read the shared opencode serve's self-reported version via `/global/health`.
///
/// Returns `None` on any failure (unreachable, non-success, or missing
/// `version` field) so the skew guard fails open — a probe failure must never
/// block an attach. Mirrors the health parse in [`setup_shared_serve_session`],
/// which keeps its own copy because it must instead bail when the serve is down.
async fn opencode_serve_version(client: &reqwest::Client, port: u16) -> Option<String> {
    let resp = client
        .get(format!("http://127.0.0.1:{port}/global/health"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Detect serve/attach-client version skew for the shared opencode serve.
///
/// Returns `Some((serve_version, client_version))` only when both versions are
/// known and incompatible; `None` otherwise (compatible, or either version
/// unreadable — fail open so the attach still runs). Used to guard the reuse
/// and respawn attach paths, matching the fresh-create guard in
/// [`setup_shared_serve_session`].
async fn opencode_attach_skew(client: &reqwest::Client, port: u16) -> Option<(String, String)> {
    let client_version = opencode_client_version()?;
    let serve_version = opencode_serve_version(client, port).await?;
    if opencode_attach_versions_compatible(&serve_version, &client_version) {
        None
    } else {
        Some((serve_version, client_version))
    }
}

/// Build the shell command that prints the version-skew notice in a pane.
///
/// `serve_version` and `client_version` are attacker-influenced free text (the
/// serve's `/global/health` body and `opencode --version` stdout), so both are
/// wrapped with [`crate::scheduler::shell_escape`] and spliced in at unquoted
/// positions — the same pattern as [`opencode_attach_command`] — to keep a
/// stray quote from breaking out of the notice and injecting shell.
fn opencode_attach_skew_notice_command(
    serve_version: &str,
    client_version: &str,
    port: u16,
) -> String {
    let serve = crate::scheduler::shell_escape(serve_version);
    let client = crate::scheduler::shell_escape(client_version);
    format!(
        "clear; printf '%s\\n' \
'ouija: opencode attach skipped — version skew (serve '{serve}' vs attach client '{client}').' \
'The attach TUI would crash, so this pane is API-only. Message delivery still works.' \
'View this session'\\''s transcript: curl http://127.0.0.1:{port}/session/<id>/message' \
'Fix: align the opencode serve and attach client versions, then restart this session.'"
    )
}

/// Replace a crashing `opencode attach` with a persistent, informative notice
/// in the pane, so an operator sees the version skew instead of a Bun stack
/// trace. The session stays functional via the HTTP API.
async fn notify_pane_opencode_attach_skew(
    pane_id: &str,
    serve_version: &str,
    client_version: &str,
    port: u16,
) {
    let notice = opencode_attach_skew_notice_command(serve_version, client_version, port);
    let pane = pane_id.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = std::process::Command::new("tmux")
            .args(["send-keys", "-t", &pane, &format!(" {notice}"), "Enter"])
            .status();
    })
    .await;
}

/// Respawn a pane to a persistent skew notice instead of a crashing `opencode
/// attach`.
///
/// The respawn paths replace the pane process with `respawn-pane -k`, so — unlike
/// [`notify_pane_opencode_attach_skew`], which sends keys into a live shell — the
/// notice command must exec a shell afterwards to keep the pane open for reading.
/// Returns `Ok(true)` so callers treat the pane as handled and keep the
/// HTTP-API session registered.
async fn respawn_pane_opencode_attach_skew_notice(
    pane_id: &str,
    serve_version: &str,
    client_version: &str,
    port: u16,
    ouija_session_id: &str,
    process_incarnation: crate::daemon_protocol::SessionIncarnation,
) -> anyhow::Result<bool> {
    let notice = opencode_attach_skew_notice_command(serve_version, client_version, port);
    // Keep the pane alive after the notice prints so the operator can read it.
    let command = format!("{notice}; exec \"${{SHELL:-/bin/sh}}\"");
    let pane = pane_id.to_string();
    let env_args = crate::tmux::pane_env_args(ouija_session_id, None, Some(process_incarnation));
    tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let mut args: Vec<&str> = vec!["respawn-pane", "-k"];
        args.extend(env_args.iter().map(String::as_str));
        args.extend_from_slice(&["-t", &pane, &command]);
        crate::tmux::configure_managed_pane(&pane);
        let status = std::process::Command::new("tmux").args(&args).status()?;
        if !status.success() {
            anyhow::bail!("tmux respawn-pane (skew notice) failed for {pane}");
        }
        Ok(true)
    })
    .await
    .map_err(|e| anyhow::anyhow!("opencode attach skew-notice respawn task failed: {e}"))?
}

#[allow(clippy::too_many_arguments)]
async fn respawn_opencode_attach_for_session(
    state: &AppState,
    claim_owner: &crate::daemon_protocol::ResourceOwner,
    process_owner: &crate::daemon_protocol::ResourceOwner,
    pane_id: &str,
    project_dir: &str,
    session_id: &str,
    port: u16,
    ouija_session_id: &str,
) -> anyhow::Result<bool> {
    state
        .with_owned_pane_claim(claim_owner, pane_id, || async {
            respawn_opencode_attach_for_session_unchecked(
                pane_id,
                project_dir,
                session_id,
                port,
                ouija_session_id,
                process_owner.incarnation,
                &state.http_client,
            )
            .await
        })
        .await
        .unwrap_or_else(|| {
            Err(anyhow::anyhow!(
                "session '{}' incarnation {} no longer owns pane '{pane_id}'",
                claim_owner.session_id,
                claim_owner.incarnation
            ))
        })
}

async fn respawn_opencode_attach_for_session_unchecked(
    pane_id: &str,
    project_dir: &str,
    session_id: &str,
    port: u16,
    ouija_session_id: &str,
    process_incarnation: crate::daemon_protocol::SessionIncarnation,
    http_client: &reqwest::Client,
) -> anyhow::Result<bool> {
    // Guard against serve/attach-client version skew before respawning: a
    // mismatched attach TUI crashes to a bare Bun stack trace. Show the notice
    // instead and keep the session functional over HTTP (mirrors the
    // fresh-create guard in setup_shared_serve_session).
    if let Some((serve_v, client_v)) = opencode_attach_skew(http_client, port).await {
        tracing::warn!(
            port,
            pane = %pane_id,
            backend_session_id = %session_id,
            serve_version = %serve_v,
            attach_client_version = %client_v,
            "opencode attach client/serve version skew on respawn; showing notice instead of attach TUI (would crash). Session remains functional via HTTP API."
        );
        return respawn_pane_opencode_attach_skew_notice(
            pane_id,
            &serve_v,
            &client_v,
            port,
            ouija_session_id,
            process_incarnation,
        )
        .await;
    }

    let attach_cmd = opencode_attach_command(port, session_id, project_dir);
    let pane = pane_id.to_string();
    let wait_pane = pane_id.to_string();
    let env_args = crate::tmux::pane_env_args(ouija_session_id, None, Some(process_incarnation));
    tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let mut args: Vec<&str> = vec!["respawn-pane", "-k"];
        args.extend(env_args.iter().map(String::as_str));
        args.extend_from_slice(&["-t", &pane, &attach_cmd]);
        crate::tmux::configure_managed_pane(&pane);
        let status = std::process::Command::new("tmux").args(&args).status()?;
        if !status.success() {
            anyhow::bail!("tmux respawn-pane failed for {pane}");
        }
        Ok(wait_for_opencode_attach(
            &wait_pane,
            std::time::Duration::from_secs(5),
        ))
    })
    .await
    .map_err(|e| anyhow::anyhow!("opencode attach respawn task failed: {e}"))?
}

async fn launch_opencode_attach_for_session(
    pane_id: &str,
    project_dir: &str,
    session_id: &str,
    port: u16,
) -> anyhow::Result<bool> {
    let attach_cmd = opencode_attach_command(port, session_id, project_dir);
    let pane = pane_id.to_string();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // Small delay so the pane shell is ready
        std::thread::sleep(std::time::Duration::from_millis(300));
        let attach_then_exit = crate::tmux::close_shell_after(&attach_cmd);
        let hidden = format!(" {attach_then_exit}");
        let status = std::process::Command::new("tmux")
            .args(["send-keys", "-t", &pane, &hidden, "Enter"])
            .status()?;
        if !status.success() {
            anyhow::bail!("tmux send-keys failed while launching opencode attach in pane {pane}");
        }
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("opencode attach launch task failed: {e}"))??;

    let pane = pane_id.to_string();
    Ok(tokio::task::spawn_blocking(move || {
        wait_for_opencode_attach(&pane, std::time::Duration::from_secs(5))
    })
    .await
    .unwrap_or(false))
}

async fn delete_opencode_session(
    client: &reqwest::Client,
    port: u16,
    session_id: &str,
    context: &str,
) -> bool {
    let url = format!("http://127.0.0.1:{port}/session/{session_id}");
    match client
        .delete(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            tracing::debug!("{context}: deleted opencode session {session_id}");
            true
        }
        Ok(r) => {
            tracing::warn!(
                "{context}: DELETE /session/{session_id} returned {}",
                r.status()
            );
            false
        }
        Err(e) => {
            tracing::warn!("{context}: DELETE /session/{session_id} failed: {e}");
            false
        }
    }
}

async fn delete_owned_opencode_session(
    state: &std::sync::Arc<AppState>,
    owner: crate::daemon_protocol::ResourceOwner,
    port: u16,
    session_id: &str,
    context: &str,
) -> bool {
    let client = state.http_client.clone();
    let session_id = session_id.to_string();
    let context = context.to_string();
    let session_id_for_guard = session_id.clone();
    state
        .with_owned_backend_cleanup(&owner, &session_id_for_guard, move || async move {
            delete_opencode_session(&client, port, &session_id, &context).await
        })
        .await
        .unwrap_or(false)
}

async fn setup_shared_serve_session(
    state: &std::sync::Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
    restart_lease_owner: Option<&crate::daemon_protocol::ResourceOwner>,
    pane_id: &str,
    project_dir: &str,
) -> anyhow::Result<String> {
    let port = state.opencode_serve_port();
    tracing::info!(
        pane = %pane_id,
        project_dir,
        port,
        "opencode shared serve setup: starting"
    );

    // Health check: verify serve is reachable
    let health = state
        .http_client
        .get(format!("http://127.0.0.1:{port}/global/health"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await;
    // The serve reports its own build version here (e.g. {"version":"1.14.31"}),
    // which we later compare against the local attach client to detect skew.
    let serve_version: Option<String>;
    match health {
        Ok(resp) if resp.status().is_success() => {
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            serve_version = body
                .get("version")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            tracing::info!(port, status = %status, serve_version = ?serve_version, "opencode shared serve health ok");
        }
        Ok(resp) => {
            tracing::warn!(port, status = %resp.status(), "opencode shared serve health returned non-success");
            anyhow::bail!(
                "opencode serve health check failed on port {port}: {}",
                resp.status()
            );
        }
        Err(e) => {
            tracing::warn!(port, error = %e, "opencode shared serve health request failed");
            anyhow::bail!(
                "opencode serve not running on port {port}. Start it with:\n  opencode serve --port {port}"
            );
        }
    }

    // Create session via HTTP API
    tracing::info!(
        port,
        project_dir,
        "opencode shared serve session create: posting"
    );
    let resp = state
        .http_client
        .post(format!("http://127.0.0.1:{port}/session"))
        .header("x-opencode-directory", project_dir)
        .json(&serde_json::json!({}))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("opencode session creation failed {status}: {body}");
    }
    let body: serde_json::Value = resp.json().await?;
    let session_id = body["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no session id in opencode response"))
        .and_then(validate_created_opencode_session_id)?;
    if let Some(lease_owner) = restart_lease_owner {
        match state
            .record_restart_backend_claim(lease_owner, owner, "opencode".into(), session_id.clone())
            .await
        {
            Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
            Ok(outcome) => {
                delete_owned_opencode_session(
                    state,
                    owner.clone(),
                    port,
                    &session_id,
                    "unclaimed shared serve cleanup",
                )
                .await;
                anyhow::bail!(
                    "restart backend claim was superseded after OpenCode creation ({outcome:?})"
                );
            }
            Err(error) => {
                delete_owned_opencode_session(
                    state,
                    owner.clone(),
                    port,
                    &session_id,
                    "unpersisted shared serve cleanup",
                )
                .await;
                return Err(error.context("failed to persist restart backend claim"));
            }
        }
    }

    tracing::info!(
        port,
        project_dir,
        pane = %pane_id,
        opencode_session_id = %session_id,
        "opencode shared serve session create: ok"
    );

    // Guard against serve/attach-client version skew. A mismatched attach TUI
    // crashes instantly and leaves a bare pane; skip it and leave an
    // informative notice instead. The session is already created and stays
    // reachable over HTTP, so we register it API-only rather than failing the
    // spawn.
    if let (Some(serve_v), Some(client_v)) = (serve_version.as_deref(), opencode_client_version()) {
        if !opencode_attach_versions_compatible(serve_v, &client_v) {
            tracing::warn!(
                port,
                pane = %pane_id,
                opencode_session_id = %session_id,
                serve_version = serve_v,
                attach_client_version = %client_v,
                "opencode attach client/serve version skew; skipping attach TUI (would crash). Session remains functional via HTTP API."
            );
            notify_pane_opencode_attach_skew(pane_id, serve_v, &client_v, port).await;
            return Ok(session_id);
        }
    }

    let attach_ready =
        match launch_opencode_attach_for_session(pane_id, project_dir, &session_id, port).await {
            Ok(ready) => ready,
            Err(e) => {
                cleanup_shared_serve_session(
                    state,
                    restart_lease_owner,
                    owner,
                    port,
                    &session_id,
                    "shared serve attach cleanup",
                )
                .await;
                return Err(e);
            }
        };

    match shared_serve_session_after_attach(session_id.clone(), attach_ready, pane_id) {
        Ok(session_id) => Ok(session_id),
        Err(e) => {
            cleanup_shared_serve_session(
                state,
                restart_lease_owner,
                owner,
                port,
                &session_id,
                "shared serve attach cleanup",
            )
            .await;
            Err(e)
        }
    }
}

async fn cleanup_shared_serve_session(
    state: &std::sync::Arc<AppState>,
    restart_lease_owner: Option<&crate::daemon_protocol::ResourceOwner>,
    target_owner: &crate::daemon_protocol::ResourceOwner,
    port: u16,
    backend_session_id: &str,
    context: &str,
) -> bool {
    match restart_lease_owner {
        Some(lease_owner) => {
            delete_claimed_restart_backend(
                state,
                lease_owner,
                target_owner,
                port,
                backend_session_id,
                context,
            )
            .await
        }
        None => {
            delete_owned_opencode_session(
                state,
                target_owner.clone(),
                port,
                backend_session_id,
                context,
            )
            .await
        }
    }
}

/// Inject a prompt into a pane after a short delay, giving the backend time to start.
/// For HttpApi backends, queue the prompt and wait for a readiness signal from the plugin.
/// Count commits `branch` is ahead of `base` inside `wt_dir`, via
/// `git rev-list --count <base>..<branch>`. Returns `None` when the subprocess
/// fails (e.g. either ref is missing), `Some(n)` on success.
fn git_rev_count(wt_dir: &str, base: &str, branch: &str) -> Option<u32> {
    let range = format!("{base}..{branch}");
    let out = std::process::Command::new("git")
        .args(["-C", wt_dir, "rev-list", "--count", &range])
        .output()
        .ok()?;
    if !out.status.success() {
        tracing::debug!(
            "git rev-list --count {range} in {wt_dir} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Resolve `ref_name` to a SHA inside `wt_dir` via `git rev-parse`. Returns
/// `None` on failure.
fn git_rev_parse(wt_dir: &str, ref_name: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", wt_dir, "rev-parse", ref_name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Reset `branch_name` to `base` inside the worktree at `wt_dir`. On
/// success, logs an info line and returns `Ok(())`. On failure, logs
/// the stderr at WARN and returns an `Err` describing the failure so
/// callers that opted in with `force_reset=true` can propagate the
/// failure rather than returning a misleading `Ok(wt_dir)` (hub#528
/// followup: `Ok(wt_dir)` after a failed reset is indistinguishable
/// from a successful reset).
fn run_reset(wt_dir: &str, branch_name: &str, base: &str) -> anyhow::Result<()> {
    let out = std::process::Command::new("git")
        .args(["-C", wt_dir, "checkout", "-B", branch_name, base])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            tracing::info!("worktree {wt_dir}: reset branch {branch_name} to {base}");
            Ok(())
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            tracing::warn!(
                "worktree {wt_dir}: git checkout -B {branch_name} {base} failed: {stderr}"
            );
            Err(anyhow::anyhow!(
                "git checkout -B {branch_name} {base} in {wt_dir} failed: {stderr}"
            ))
        }
        Err(e) => {
            tracing::warn!(
                "worktree {wt_dir}: failed to spawn git checkout -B {branch_name} {base}: {e}"
            );
            Err(anyhow::anyhow!(
                "failed to spawn git checkout -B {branch_name} {base} in {wt_dir}: {e}"
            ))
        }
    }
}

fn add_existing_branch_worktree(repo_dir: &str, wt_dir: &str, branch: &str) -> anyhow::Result<()> {
    let output = std::process::Command::new("git")
        .args(["-C", repo_dir, "worktree", "add", wt_dir, branch])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Return a warning message when a legacy-layout worktree short-circuit
/// will silently drop a caller's destructive intent.
///
/// The legacy path (`<repo>/.ouija/worktrees/<name>`) is returned
/// as-is for running-session compatibility — no `git checkout -B` is
/// run, even when the caller passed `force_reset=true` with a
/// `base_branch`. This matches the explicit design of the legacy
/// short-circuit, but it makes `force_reset=true` unobservably dropped.
///
/// Returns `None` when nothing was dropped (no `base_branch`, or
/// `force_reset=false` where the new path would also skip). Returns
/// `Some(msg)` to be logged at WARN when the opt-in is silenced.
/// Caller emits the log; predicate is pure for unit testing.
fn legacy_drops_destructive_intent(base_branch: Option<&str>, force_reset: bool) -> Option<String> {
    if !force_reset || base_branch.is_none() {
        return None;
    }
    let base = base_branch.unwrap();
    Some(format!(
        "legacy-layout worktree short-circuit: force_reset=true + base_branch={base} \
         silently dropped (legacy path returns the dir as-is for running-session \
         compatibility). If destructive intent was load-bearing here, migrate the \
         worktree to the new <home>/.ouija/worktrees/ layout."
    ))
}

/// Create an ouija-managed git worktree at `<home>/.ouija/worktrees/<repo-slug>/<name>`.
///
/// Worktrees live outside the repo directory tree to prevent Claude Code from
/// resolving the `.git` pointer back to the main repo and editing files there.
///
/// Falls back to legacy `<repo>/.ouija/worktrees/<name>` if that directory
/// already exists (avoids breaking running sessions).
///
/// When the worktree dir already exists and `base_branch` is `Some`, the
/// function will reset the branch to base only when safe:
/// - the branch is not ahead of base, or
/// - `force_reset` is `true` (explicit caller opt-in).
///
/// When the branch is ahead and `force_reset` is `false`, the reset is
/// skipped and a structured warning is logged so the caller can recover via
/// `git reflog`. This guards against silent data loss (hub#528).
#[cfg(test)]
fn create_ouija_worktree(
    repo_dir: &str,
    name: &str,
    branch: Option<&str>,
    base_branch: Option<&str>,
    force_reset: bool,
    home: &std::path::Path,
) -> anyhow::Result<String> {
    let wt_dir = ouija_worktree_dir(repo_dir, name, home);
    create_ouija_worktree_at(repo_dir, name, branch, base_branch, force_reset, &wt_dir)
}

fn create_ouija_worktree_at(
    repo_dir: &str,
    name: &str,
    branch: Option<&str>,
    base_branch: Option<&str>,
    force_reset: bool,
    wt_dir: &str,
) -> anyhow::Result<String> {
    let legacy_dir = format!("{repo_dir}/.ouija/worktrees/{name}");
    if wt_dir == legacy_dir {
        if let Some(msg) = legacy_drops_destructive_intent(base_branch, force_reset) {
            // Mirror the non-legacy arms (Some(0)/Some(n)/None at
            // :2612/:2626/:2640): when force_reset=true is asserted but
            // cannot be honored here, return Err so Ok(wt_dir) never
            // conflates "reset happened" with "reset was silently
            // dropped". Warn-log too so the reason is in daemon logs.
            tracing::warn!("worktree {name}: {msg}");
            return Err(anyhow::anyhow!(msg));
        }
        return Ok(legacy_dir);
    }
    if std::path::Path::new(wt_dir).exists() {
        // If base_branch is specified, the caller may want the branch reset
        // to base. This is data-destructive: if the branch is ahead of base
        // (has real commits), an unconditional reset silently discards those
        // commits (hub#528 regression). Guard against that: only reset when
        // the branch is not ahead of base, OR the caller explicitly opts in
        // with `force_reset`.
        if let Some(base) = base_branch {
            let branch_name = branch.unwrap_or(name);
            let ahead = git_rev_count(wt_dir, base, branch_name);
            match ahead {
                Some(0) => {
                    // Zero ahead: nothing to lose. Still run the reset so
                    // the working tree and HEAD are aligned (handles cases
                    // where the branch existed but was pointed elsewhere).
                    //
                    // When `force_reset=true`, the caller explicitly
                    // asserted they want the reset; propagate failures
                    // so they see when their request was not honored
                    // (matches the Some(n>0) and None force_reset=true
                    // arms below).
                    //
                    // When `force_reset=false`, the reset is a best-effort
                    // alignment convenience that must not block session
                    // start on a transient git failure. Log the outcome
                    // honestly: on failure, warn; only emit the
                    // "no-op" info line when the reset actually ran.
                    match run_reset(wt_dir, branch_name, base) {
                        Ok(()) => {
                            tracing::info!(
                                "worktree {name}: branch {branch_name} is 0 commits ahead of {base}, reset is a no-op"
                            );
                        }
                        Err(e) if force_reset => {
                            return Err(e);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "worktree {name}: branch {branch_name} is 0 commits ahead of {base} \
                                 but alignment reset failed: {e}. Continuing without force_reset."
                            );
                        }
                    }
                }
                Some(n) if n > 0 && !force_reset => {
                    let tip = git_rev_parse(wt_dir, branch_name).unwrap_or_else(|| "?".into());
                    tracing::warn!(
                        "worktree {name}: SKIPPING reset of branch {branch_name} to {base} \
                         because it is {n} commits ahead (tip {tip}); \
                         pass force_reset=true to override. \
                         Recover via `git -C {wt_dir} reflog` if the branch was lost."
                    );
                    // Do NOT reset. Return the worktree as-is.
                }
                Some(n) => {
                    // force_reset is true and n > 0: record what we are
                    // about to discard so reflog recovery is discoverable,
                    // then propagate any reset failure so the caller sees
                    // their explicit destructive request was not honored.
                    let tip = git_rev_parse(wt_dir, branch_name).unwrap_or_else(|| "?".into());
                    tracing::warn!(
                        "worktree {name}: force_reset=true, DISCARDING {n} commits on branch {branch_name} (tip {tip}) to reset to {base}"
                    );
                    run_reset(wt_dir, branch_name, base)?;
                }
                None if force_reset => {
                    // `git rev-list` failed — the base ref might not exist
                    // in this worktree, or the branch does not yet exist.
                    // Since the caller explicitly opted in with
                    // `force_reset=true`, honor the intent rather than
                    // silently dropping it. Propagate a reset failure so
                    // the caller does not receive a misleading Ok(wt_dir)
                    // while their destructive request was dropped.
                    tracing::warn!(
                        "worktree {name}: cannot compute {base}..{branch_name} commit count \
                         (base or branch ref missing), but force_reset=true — attempting reset anyway"
                    );
                    run_reset(wt_dir, branch_name, base)?;
                }
                None => {
                    // `git rev-list` failed and force_reset is false —
                    // fail safe: skip the reset, warn so operators can
                    // see why the reset did not happen.
                    tracing::warn!(
                        "worktree {name}: cannot compute {base}..{branch_name} commit count \
                         (base or branch ref missing); skipping reset to avoid data loss. \
                         Pass force_reset=true to override."
                    );
                }
            }
        }
        return Ok(wt_dir.to_string());
    }
    // Ensure parent dir exists
    let parent = std::path::Path::new(wt_dir)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("worktree path has no parent: {wt_dir}"))?;
    std::fs::create_dir_all(parent)?;
    // Create worktree with a new branch
    let branch = branch.map(String::from).unwrap_or_else(|| name.to_string());
    if let Some(base) = base_branch {
        if !force_reset && git_rev_parse(repo_dir, &branch).is_some() {
            let ahead = git_rev_count(repo_dir, base, &branch);
            match ahead {
                Some(n) if n > 0 => {
                    let tip = git_rev_parse(repo_dir, &branch).unwrap_or_else(|| "?".into());
                    tracing::warn!(
                        "worktree {name}: creating missing worktree from existing branch {branch} \
                         without reset because it is {n} commits ahead of {base} (tip {tip}); \
                         pass force_reset=true to override"
                    );
                    add_existing_branch_worktree(repo_dir, wt_dir, &branch)?;
                    return Ok(wt_dir.to_string());
                }
                None => {
                    tracing::warn!(
                        "worktree {name}: cannot compute {base}..{branch} commit count \
                         before creating missing worktree; checking out existing branch without \
                         reset to avoid data loss. Pass force_reset=true to override."
                    );
                    add_existing_branch_worktree(repo_dir, wt_dir, &branch)?;
                    return Ok(wt_dir.to_string());
                }
                _ => {}
            }
        }
    }
    let flag = if base_branch.is_some() { "-B" } else { "-b" };
    let mut args = vec!["-C", repo_dir, "worktree", "add", flag, &branch, wt_dir];
    if let Some(base) = base_branch {
        args.push(base);
    }
    let output = std::process::Command::new("git").args(&args).output()?;
    if !output.status.success() {
        if base_branch.is_some() && force_reset {
            anyhow::bail!(
                "git worktree add -B failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        // Branch might already exist — check it out in the worktree
        add_existing_branch_worktree(repo_dir, wt_dir, &branch)?;
    }
    Ok(wt_dir.to_string())
}

/// Resolve the one project-directory gate a managed worktree must claim
/// before create/reset I/O begins.
#[cfg(test)]
fn ouija_worktree_dir(repo_dir: &str, name: &str, home: &std::path::Path) -> String {
    let [legacy_dir, new_dir] = ouija_worktree_candidates(repo_dir, name, home);
    if std::path::Path::new(&legacy_dir).exists() {
        legacy_dir
    } else {
        new_dir
    }
}

fn ouija_worktree_candidates(repo_dir: &str, name: &str, home: &std::path::Path) -> [String; 2] {
    let legacy_dir = format!("{repo_dir}/.ouija/worktrees/{name}");
    let repo_slug = std::path::Path::new(repo_dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let new_dir = format!(
        "{}/.ouija/worktrees/{repo_slug}/{name}",
        home.to_string_lossy()
    );
    [legacy_dir, new_dir]
}

/// Queue a prompt for HttpApi session delivery via readiness signal.
///
/// TuiInjection sessions pass prompts as CLI args instead — this function
/// should only be called for HttpApi backends.
#[cfg(test)]
pub(crate) fn schedule_prompt_injection(
    state: &std::sync::Arc<AppState>,
    session_name: &str,
    pane_id: String,
    prompt: String,
    backend_session_id: Option<String>,
) {
    schedule_prompt_injection_for_owner(
        state,
        session_name,
        pane_id,
        prompt,
        backend_session_id,
        None,
    );
}

pub(crate) fn schedule_prompt_injection_owned(
    state: &std::sync::Arc<AppState>,
    session_name: &str,
    pane_id: String,
    prompt: String,
    backend_session_id: Option<String>,
    owner: crate::daemon_protocol::ResourceOwner,
) {
    schedule_prompt_injection_for_owner(
        state,
        session_name,
        pane_id,
        prompt,
        backend_session_id,
        Some(owner),
    );
}

fn schedule_prompt_injection_for_owner(
    state: &std::sync::Arc<AppState>,
    session_name: &str,
    pane_id: String,
    prompt: String,
    backend_session_id: Option<String>,
    owner: Option<crate::daemon_protocol::ResourceOwner>,
) {
    // Queue prompt synchronously so the plugin's readiness signal finds it.
    let pending = crate::state::PendingPrompt::new(
        pane_id.clone(),
        prompt.clone(),
        backend_session_id.clone(),
    );
    let pending = match owner {
        Some(owner) => pending.with_owner(owner),
        None => pending,
    };
    state
        .pending_prompts
        .lock()
        .unwrap()
        .insert(session_name.to_string(), pending);

    // Fallback timer: if readiness signal doesn't arrive within 10s,
    // deliver via tmux injection.
    let name = session_name.to_string();
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(PENDING_PROMPT_FALLBACK_DELAY).await;
        let pending = reserve_pending_prompt_if_matches(
            &state,
            &name,
            &pane_id,
            &prompt,
            backend_session_id.as_deref(),
        );
        if let Some(pending) = pending {
            if !pending_prompt_owner_is_current(&state, &name, &pending).await {
                tracing::info!("discarding superseded readiness prompt for {name}");
                return;
            }
            tracing::info!("readiness timeout for {name}, delivering prompt via fallback");
            match deliver_prompt_fallback(
                &state,
                &name,
                &pending.pane_id,
                &pending.prompt,
                true,
                false,
                pending.backend_session_id.as_deref(),
                pending.owner.as_ref(),
            )
            .await
            {
                Ok(()) => {}
                Err(error) => {
                    if !pending_prompt_owner_is_current(&state, &name, &pending).await {
                        tracing::info!(
                            "discarding readiness prompt superseded during fallback for {name}"
                        );
                        return;
                    }
                    restore_pending_prompt_if_absent(&state, &name, pending.clone());
                    schedule_pending_prompt_fallback_retry(&state, &name, pending, true);
                    tracing::warn!("readiness timeout fallback failed for {name}: {error}");
                }
            }
        }
    });
}

#[cfg(test)]
const PENDING_PROMPT_FALLBACK_DELAY: std::time::Duration = std::time::Duration::from_millis(10);
#[cfg(not(test))]
const PENDING_PROMPT_FALLBACK_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
const PENDING_PROMPT_MAX_FALLBACK_RETRIES: u8 = 3;

fn schedule_pending_prompt_fallback_retry(
    state: &std::sync::Arc<AppState>,
    session_name: &str,
    pending_prompt: crate::state::PendingPrompt,
    is_http_api: bool,
) {
    schedule_pending_prompt_fallback_retry_attempt(
        state,
        session_name,
        pending_prompt,
        is_http_api,
        1,
    );
}

fn schedule_pending_prompt_fallback_retry_attempt(
    state: &std::sync::Arc<AppState>,
    session_name: &str,
    pending_prompt: crate::state::PendingPrompt,
    is_http_api: bool,
    attempt: u8,
) {
    let state = state.clone();
    let session_name = session_name.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(PENDING_PROMPT_FALLBACK_DELAY).await;
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
        if !pending_prompt_owner_is_current(&state, &session_name, &pending).await {
            tracing::info!("discarding superseded readiness retry for {session_name}");
            return;
        }

        match deliver_prompt_fallback(
            &state,
            &session_name,
            &pending.pane_id,
            &pending.prompt,
            is_http_api,
            false,
            pending.backend_session_id.as_deref(),
            pending.owner.as_ref(),
        )
        .await
        {
            Ok(()) => {}
            Err(error) => {
                if !pending_prompt_owner_is_current(&state, &session_name, &pending).await {
                    tracing::info!(
                        "discarding readiness retry superseded during fallback for {session_name}"
                    );
                    return;
                }
                restore_pending_prompt_if_absent(&state, &session_name, pending.clone());
                if attempt < PENDING_PROMPT_MAX_FALLBACK_RETRIES {
                    schedule_pending_prompt_fallback_retry_attempt(
                        &state,
                        &session_name,
                        pending,
                        is_http_api,
                        attempt + 1,
                    );
                }
                tracing::warn!(
                    "readiness timeout fallback retry attempt {attempt}/{PENDING_PROMPT_MAX_FALLBACK_RETRIES} failed for {session_name}: {error}"
                );
            }
        }
    });
}

fn reserve_pending_prompt_if_matches(
    state: &std::sync::Arc<AppState>,
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

async fn pending_prompt_owner_is_current(
    state: &AppState,
    session_name: &str,
    pending: &crate::state::PendingPrompt,
) -> bool {
    let Some(owner) = pending.owner.as_ref() else {
        return true;
    };
    state
        .protocol
        .read()
        .await
        .sessions
        .get(session_name)
        .is_some_and(|session| session.owner() == *owner)
}

fn restore_pending_prompt_if_absent(
    state: &std::sync::Arc<AppState>,
    session_name: &str,
    pending_prompt: crate::state::PendingPrompt,
) {
    state
        .pending_prompts
        .lock()
        .unwrap()
        .entry(session_name.to_string())
        .or_insert(pending_prompt);
}

fn restore_start_prompt_after_fallback_failure(
    state: &std::sync::Arc<AppState>,
    session_name: &str,
    pending_prompt: crate::state::PendingPrompt,
) {
    restore_pending_prompt_if_absent(state, session_name, pending_prompt.clone());
    schedule_pending_prompt_fallback_retry(state, session_name, pending_prompt, true);
}

fn restore_restart_prompt_after_fallback_failure(
    state: &std::sync::Arc<AppState>,
    session_name: &str,
    pending_prompt: crate::state::PendingPrompt,
) {
    restore_start_prompt_after_fallback_failure(state, session_name, pending_prompt);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartPromptDelivery {
    PromptAsync,
    AlreadyPassedAsCliArg,
    Unavailable,
}

fn start_prompt_delivery(
    is_http_api: bool,
    backend_session_id: Option<&str>,
) -> StartPromptDelivery {
    if !is_http_api {
        StartPromptDelivery::AlreadyPassedAsCliArg
    } else if backend_session_id.is_some() {
        StartPromptDelivery::PromptAsync
    } else {
        StartPromptDelivery::Unavailable
    }
}

fn start_prompt_msg_id(msg_id: Option<u64>, delivery: Option<StartPromptDelivery>) -> Option<u64> {
    match delivery {
        Some(StartPromptDelivery::Unavailable) => None,
        _ => msg_id,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptFallbackDelivery {
    RawTmux,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptAsyncFallbackDecision {
    DefiniteNonAcceptance,
    Ambiguous,
}

impl PromptAsyncFallbackDecision {
    pub(crate) fn should_try_raw_tmux(self) -> bool {
        matches!(self, Self::DefiniteNonAcceptance)
    }
}

pub(crate) enum PromptAsyncFailure<'a> {
    Status(reqwest::StatusCode),
    Request(&'a reqwest::Error),
}

fn prompt_fallback_delivery() -> PromptFallbackDelivery {
    PromptFallbackDelivery::RawTmux
}

fn should_deliver_prompt_fallback(is_http_api: bool, opencode_tui_alive: bool) -> bool {
    !is_http_api || opencode_tui_alive
}

pub(crate) fn classify_prompt_async_fallback(
    failure: PromptAsyncFailure<'_>,
) -> PromptAsyncFallbackDecision {
    match failure {
        PromptAsyncFailure::Status(
            reqwest::StatusCode::BAD_REQUEST
            | reqwest::StatusCode::NOT_FOUND
            | reqwest::StatusCode::CONFLICT
            | reqwest::StatusCode::GONE
            | reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        ) => PromptAsyncFallbackDecision::DefiniteNonAcceptance,
        PromptAsyncFailure::Request(error) if error.is_connect() => {
            PromptAsyncFallbackDecision::DefiniteNonAcceptance
        }
        PromptAsyncFailure::Status(_) | PromptAsyncFailure::Request(_) => {
            PromptAsyncFallbackDecision::Ambiguous
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn deliver_prompt_fallback(
    state: &AppState,
    session_id: &str,
    pane: &str,
    text: &str,
    is_http_api: bool,
    vim_mode: bool,
    expected_backend_session_id: Option<&str>,
    expected_owner: Option<&crate::daemon_protocol::ResourceOwner>,
) -> anyhow::Result<()> {
    if let Some(owner) = expected_owner {
        return state
            .with_owned_pane_claim(owner, pane, || async {
                deliver_prompt_fallback_unchecked(
                    state,
                    session_id,
                    pane,
                    text,
                    is_http_api,
                    vim_mode,
                    expected_backend_session_id,
                )
                .await
            })
            .await
            .unwrap_or_else(|| {
                Err(anyhow::anyhow!(
                    "prompt fallback skipped: queued incarnation is no longer current for session {session_id}"
                ))
            });
    }
    deliver_prompt_fallback_unchecked(
        state,
        session_id,
        pane,
        text,
        is_http_api,
        vim_mode,
        expected_backend_session_id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn deliver_prompt_fallback_unchecked(
    state: &AppState,
    session_id: &str,
    pane: &str,
    text: &str,
    is_http_api: bool,
    vim_mode: bool,
    expected_backend_session_id: Option<&str>,
) -> anyhow::Result<()> {
    let (pane_still_registered, backend_session_matches) = {
        let proto = state.protocol.read().await;
        match proto.sessions.get(session_id) {
            Some(session) => (
                session.pane.as_deref() == Some(pane),
                expected_backend_session_id.is_none_or(|expected| {
                    session.metadata.backend_session_id.as_deref() == Some(expected)
                }),
            ),
            None => (false, false),
        }
    };
    if !pane_still_registered {
        anyhow::bail!(
            "prompt fallback skipped: pane {pane} is no longer registered to session {session_id}"
        );
    }
    if !backend_session_matches {
        anyhow::bail!(
            "prompt fallback skipped: queued OpenCode backend session is no longer current for session {session_id}"
        );
    }

    let opencode_tui_alive = !is_http_api || crate::tmux::pane_alive(pane, &["opencode"]);
    if !should_deliver_prompt_fallback(is_http_api, opencode_tui_alive) {
        anyhow::bail!("prompt fallback skipped: pane {pane} is no longer running an opencode TUI");
    }

    match prompt_fallback_delivery() {
        PromptFallbackDelivery::RawTmux => {
            crate::tmux::locked_inject_raw_tmux(state, session_id, pane, text, vim_mode).await
        }
    }
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

    let keys = load_or_create_keys(&state.config.config_dir)?;

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

    let transport =
        Arc::new(NostrTransport::new(keys, relay_urls, state.config.data_dir.clone()).await?);

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
        // Writing hex to a String is infallible.
        write!(s, "{b:02x}").expect("String write failed");
    }
    s
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
pub(crate) fn load_peer_pubkeys(data_dir: &Path) -> HashSet<PublicKey> {
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

/// Build an opencode `prompt_async` request body from the session's text,
/// model, and effort.
///
/// The returned JSON has `parts: [{type: "text", text}]` always present, plus
/// optional top-level `model` and `variant` fields that opencode merges into
/// its per-prompt overrides (see opencode's `prompt.ts` precedence:
/// `input.model ?? ag.model ?? lastModel(sessionID)`).
///
/// `model` is split on the **first** `/` into `providerID` / `modelID`,
/// mirroring opencode's parser at `packages/opencode/src/provider/provider.ts`.
/// `"openrouter/openai/gpt-5.4"` -> `providerID="openrouter"`, `modelID="openai/gpt-5.4"`.
///
/// A model string with no `/`, or one with an empty segment on either side
/// of the first `/` (`"/"`, `"openrouter/"`, `"/gpt-5"`, or
/// whitespace-only input), is treated as ambiguous: the `model` field is
/// omitted entirely and a `tracing::warn!` is emitted. Opencode then falls
/// back to the agent / session default. Effort is passed through unchanged
/// as `variant` — callers should normalize empty strings to `None` upstream
/// (the API boundary does this).
pub(crate) fn opencode_prompt_body(
    text: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "parts": [{"type": "text", "text": text}],
    });
    let obj = body.as_object_mut().expect("json! macro returns an object");
    if let Some(m) = model {
        let trimmed = m.trim();
        match trimmed.split_once('/') {
            Some((provider, model_id)) => {
                // Trim each segment independently: `"openrouter / gpt-5"`
                // would otherwise send `providerID: " openrouter "` which
                // opencode's provider lookup does not match. The non-empty
                // guard is then applied to the already-trimmed segments so
                // inputs like `" / "` or `"openrouter / "` are rejected.
                let provider = provider.trim();
                let model_id = model_id.trim();
                if !provider.is_empty() && !model_id.is_empty() {
                    obj.insert(
                        "model".into(),
                        serde_json::json!({
                            "providerID": provider,
                            "modelID": model_id,
                        }),
                    );
                } else {
                    tracing::warn!(
                        model = m,
                        "opencode_prompt_body: model string has empty segment after trim; falling back to agent/session default"
                    );
                }
            }
            None => {
                tracing::warn!(
                    model = m,
                    "opencode_prompt_body: model string is not in 'providerID/modelID' form; falling back to agent/session default"
                );
            }
        }
    }
    if let Some(e) = effort {
        let trimmed = e.trim();
        if !trimmed.is_empty() {
            obj.insert(
                "variant".into(),
                serde_json::Value::String(trimmed.to_string()),
            );
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_owned_kill_returns_typed_superseded_before_external_work() {
        let state = AppState::new_for_test();
        let current_owner = {
            let mut protocol = state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%2".into()),
                metadata: Default::default(),
            });
            protocol.sessions["worker"].owner()
        };
        let stale_owner = crate::daemon_protocol::ResourceOwner {
            session_id: current_owner.session_id.clone(),
            incarnation: crate::daemon_protocol::SessionIncarnation(
                current_owner.incarnation.0 + 1,
            ),
        };

        let result = kill_session_owned(&state, &stale_owner, "%2").await;

        assert_eq!(result.outcome, KillOutcome::Superseded);
        let protocol = state.protocol.read().await;
        assert_eq!(protocol.sessions["worker"].owner(), current_owner);
        assert!(
            protocol.lifecycle_leases.is_empty(),
            "a stale kill must not claim lifecycle authority"
        );
    }

    #[tokio::test]
    async fn second_kill_returns_typed_superseded_after_first_claims_owner() {
        let state = AppState::new_for_test();
        let owner = {
            let mut protocol = state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%2".into()),
                metadata: Default::default(),
            });
            protocol.sessions["worker"].owner()
        };
        assert_eq!(
            state
                .claim_existing_stop(&owner, "%2", false)
                .await
                .unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );

        let result = kill_session(&state, "worker").await;

        assert_eq!(result.outcome, KillOutcome::Superseded);
        let protocol = state.protocol.read().await;
        assert_eq!(protocol.sessions["worker"].owner(), owner);
        assert_eq!(protocol.lifecycle_leases["worker"].owner, owner);
    }

    #[tokio::test]
    async fn http_kill_without_backend_session_id_releases_unused_stop_authority() {
        let state = AppState::new_for_test();
        {
            let mut protocol = state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%2".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: None,
                    ..Default::default()
                },
            });
        }

        let result = kill_session(&state, "worker").await;

        assert_eq!(result.outcome, KillOutcome::Failed);
        let protocol = state.protocol.read().await;
        assert!(protocol.sessions.contains_key("worker"));
        assert!(
            protocol.lifecycle_leases.is_empty(),
            "an invalid HTTP kill must release its claim because no external work started"
        );
    }

    #[tokio::test]
    async fn concurrent_restart_claim_reaches_no_external_boundary() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%2".into()),
                metadata: Default::default(),
            })
            .await;

        let first = claim_restart_for_external_work(&state, "worker")
            .await
            .unwrap();
        let second = claim_restart_for_external_work(&state, "worker").await;

        assert_eq!(second, Err(RestartOutcome::Superseded));
        let protocol = state.protocol.read().await;
        assert_eq!(protocol.lifecycle_leases["worker"].owner, first);
        assert_eq!(
            protocol.lifecycle_leases["worker"].phase,
            crate::daemon_protocol::LifecyclePhase::Restarting
        );
        assert!(matches!(
            protocol.clone().reserve_start("worker").unwrap(),
            crate::daemon_protocol::StartDisposition::InProgress(owner) if owner == first
        ));
    }

    #[tokio::test]
    async fn concurrent_same_id_starts_cross_the_launch_boundary_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Barrier;

        let state = AppState::new_for_test();
        let ready = Arc::new(Barrier::new(3));
        let launch_count = Arc::new(AtomicUsize::new(0));
        let mut attempts = Vec::new();

        for _ in 0..2 {
            let state = state.clone();
            let ready = ready.clone();
            let launch_count = launch_count.clone();
            attempts.push(tokio::spawn(async move {
                ready.wait().await;
                let disposition = reserve_start_for_launch(&state, "same-id")
                    .await
                    .expect("reservation must persist");
                if matches!(
                    disposition,
                    crate::daemon_protocol::StartDisposition::Reserved(_)
                ) {
                    launch_count.fetch_add(1, Ordering::SeqCst);
                }
                disposition
            }));
        }

        ready.wait().await;
        let first = attempts.remove(0).await.unwrap();
        let second = attempts.remove(0).await.unwrap();

        assert_eq!(launch_count.load(Ordering::SeqCst), 1);
        assert!(
            matches!(
                (&first, &second),
                (
                    crate::daemon_protocol::StartDisposition::Reserved(_),
                    crate::daemon_protocol::StartDisposition::InProgress(_)
                ) | (
                    crate::daemon_protocol::StartDisposition::InProgress(_),
                    crate::daemon_protocol::StartDisposition::Reserved(_)
                )
            ),
            "exactly one attempt must own launch authority: {first:?}, {second:?}"
        );
    }

    #[tokio::test]
    async fn stale_start_success_cannot_finalize_replacement_metadata() {
        let state = AppState::new_for_test();
        let owner = match reserve_start_for_launch(&state, "same-id").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            other => panic!("expected reservation, got {other:?}"),
        };
        state
            .commit_reserved_start(
                &owner,
                Some("%old".into()),
                crate::daemon_protocol::SessionMeta::default(),
            )
            .await
            .unwrap();
        state.abort_lifecycle(&owner).await.unwrap();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "same-id".into(),
                pane: Some("%winner".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    bulletin: Some("winner".into()),
                    ..Default::default()
                },
            })
            .await;

        let outcome = finalize_reserved_start(
            &state,
            &owner,
            Some("%old".into()),
            crate::daemon_protocol::SessionMeta {
                bulletin: Some("stale".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            crate::daemon_protocol::LifecycleMutationOutcome::Superseded
        );
        let winner = &state.protocol.read().await.sessions["same-id"];
        assert_eq!(winner.pane.as_deref(), Some("%winner"));
        assert_eq!(winner.metadata.bulletin.as_deref(), Some("winner"));
    }

    #[tokio::test]
    async fn reserved_initial_start_retains_requested_active_context_policy() {
        // Break caught: the initial reserved start and its later launch
        // finalizer must both retain the policy selected from API ingress.
        let state = AppState::new_for_test();
        let owner = match reserve_start_for_launch(&state, "initial-policy")
            .await
            .unwrap()
        {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            other => panic!("expected reservation, got {other:?}"),
        };
        let selected_policy = active_context_policy_for_launch(None, Some(120), true);
        assert_eq!(
            state
                .commit_reserved_start(
                    &owner,
                    None,
                    crate::daemon_protocol::SessionMeta {
                        fresh_context_after_active_secs: selected_policy,
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        assert_eq!(
            finalize_reserved_start(
                &state,
                &owner,
                None,
                crate::daemon_protocol::SessionMeta {
                    fresh_context_after_active_secs: selected_policy,
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        state.abort_lifecycle(&owner).await.unwrap();

        let protocol = state.protocol.read().await;
        assert_eq!(
            protocol.sessions["initial-policy"]
                .metadata
                .fresh_context_after_active_secs,
            Some(120)
        );
        assert_eq!(
            protocol.sessions["initial-policy"]
                .metadata
                .active_context_accumulated_secs,
            0
        );
        assert!(
            !protocol.sessions["initial-policy"]
                .metadata
                .active_context_accounting_provisional
        );
    }

    #[tokio::test]
    async fn stale_start_failure_cannot_remove_same_pane_replacement() {
        let state = AppState::new_for_test();
        let owner = match reserve_start_for_launch(&state, "same-id").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            other => panic!("expected reservation, got {other:?}"),
        };
        state
            .commit_reserved_start(
                &owner,
                Some("%shared".into()),
                crate::daemon_protocol::SessionMeta::default(),
            )
            .await
            .unwrap();
        state.abort_lifecycle(&owner).await.unwrap();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "same-id".into(),
                pane: Some("%shared".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    bulletin: Some("winner".into()),
                    ..Default::default()
                },
            })
            .await;

        cleanup_reserved_start(&state, &owner, "%shared", None).await;

        let winner = &state.protocol.read().await.sessions["same-id"];
        assert_ne!(winner.metadata.session_incarnation, owner.incarnation);
        assert_eq!(winner.metadata.bulletin.as_deref(), Some("winner"));
    }

    #[tokio::test]
    async fn existing_start_claim_is_revalidated_before_restart_io() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "same-id".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let owner = match reserve_start_for_launch(&state, "same-id").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Existing(owner) => owner,
            other => panic!("expected existing owner, got {other:?}"),
        };
        assert_eq!(
            state.claim_existing_start(&owner).await.unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        state.protocol.write().await.sessions.remove("same-id");

        let (_, _, outcome) = restart_session_for_start(
            &state,
            &owner,
            "same-id",
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
        )
        .await;

        assert_eq!(outcome, RestartOutcome::Superseded);
        assert!(state.protocol.read().await.lifecycle_leases.is_empty());
    }

    #[tokio::test]
    async fn failed_staged_restart_without_a_pane_still_retains_recovery_lease() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "same-id".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let owner = match reserve_start_for_launch(&state, "same-id").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Existing(owner) => owner,
            other => panic!("expected existing owner, got {other:?}"),
        };
        assert_eq!(
            state.claim_existing_start(&owner).await.unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        assert!(matches!(
            state
                .stage_fresh_launch("same-id", "claude-code".into(), None, None)
                .await,
            crate::daemon_protocol::StageFreshLaunchOutcome::Staged { .. }
        ));

        let proto = state.protocol.read().await;
        assert!(restart_recovery_pending(&proto, &owner));
    }

    #[tokio::test]
    async fn missing_incumbent_pane_restarts_through_recreate_path() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "same-id".into(),
                pane: Some("%definitely-not-a-live-managed-pane".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    ..Default::default()
                },
            })
            .await;
        let previous = state.protocol.read().await.sessions["same-id"].clone();
        let owner = match reserve_start_for_launch(&state, "same-id").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Existing(owner) => owner,
            other => panic!("expected existing owner, got {other:?}"),
        };
        assert_eq!(
            state.claim_existing_start(&owner).await.unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );

        let (message, _, outcome) = restart_session_for_start(
            &state,
            &owner,
            "same-id",
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
        )
        .await;

        assert_eq!(outcome, RestartOutcome::Restarted, "{message}");
        let proto = state.protocol.read().await;
        let restarted = &proto.sessions["same-id"];
        assert_ne!(restarted.owner(), previous.owner());
        assert_ne!(
            restarted.pane.as_deref(),
            previous.pane.as_deref(),
            "a missing incumbent must select the new-pane fallback"
        );
        assert!(proto.lifecycle_leases.is_empty());
    }

    #[tokio::test]
    async fn hard_fresh_restart_applies_active_context_policy_only_after_completion() {
        // Break caught: a successful hard fresh restart must apply the requested
        // limit and reset the previous incarnation's active-time accounting.
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "hard-fresh".into(),
                pane: Some("%definitely-not-a-live-managed-pane".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    prompt: Some("stored continuation".into()),
                    fresh_context_after_active_secs: Some(60),
                    active_context_accumulated_secs: 61,
                    active_context_segment_started_at: Some(100),
                    active_context_restart_due: true,
                    ..Default::default()
                },
            })
            .await;

        let (message, _, outcome) = restart_session_with_prompt_controls(
            &state,
            "hard-fresh",
            true,
            Some(120),
            None,
            None,
            false,
            Some("one-shot continuation"),
            None,
            None,
            Some("claude-code"),
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
        )
        .await;

        assert_eq!(outcome, RestartOutcome::Restarted, "{message}");
        let protocol = state.protocol.read().await;
        let metadata = &protocol.sessions["hard-fresh"].metadata;
        assert_eq!(metadata.fresh_context_after_active_secs, Some(120));
        assert_eq!(metadata.active_context_accumulated_secs, 0);
        assert_eq!(metadata.active_context_segment_started_at, None);
        assert!(!metadata.active_context_restart_due);
        assert!(!metadata.active_context_accounting_provisional);
        assert_eq!(metadata.prompt.as_deref(), Some("stored continuation"));
    }

    #[tokio::test]
    async fn nonfresh_restart_with_new_backend_identity_preserves_active_context() {
        // Break caught: backend-identity replacement is not itself a fresh
        // context restart and must not reset or replace active-time state.
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "nonfresh-recovery".into(),
                pane: Some("%definitely-not-a-live-managed-pane".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    backend_session_id: None,
                    fresh_context_after_active_secs: Some(60),
                    active_context_accumulated_secs: 41,
                    active_context_segment_started_at: Some(100),
                    active_context_restart_due: true,
                    ..Default::default()
                },
            })
            .await;

        let owner = state.protocol.read().await.sessions["nonfresh-recovery"].owner();
        assert_eq!(
            state.claim_existing_start(&owner).await.unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        let (message, _, outcome) = restart_session_for_start_with_active_context_policy(
            &state,
            &owner,
            "nonfresh-recovery",
            false,
            None,
            None,
            None,
            None,
            None,
            Some("claude-code"),
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
        )
        .await;

        assert_eq!(outcome, RestartOutcome::Restarted, "{message}");
        let protocol = state.protocol.read().await;
        let metadata = &protocol.sessions["nonfresh-recovery"].metadata;
        assert_ne!(metadata.session_incarnation, owner.incarnation);
        assert_eq!(metadata.fresh_context_after_active_secs, Some(60));
        assert_eq!(metadata.active_context_accumulated_secs, 41);
        assert_eq!(metadata.active_context_segment_started_at, Some(100));
        assert!(metadata.active_context_restart_due);
        assert!(!metadata.active_context_accounting_provisional);
    }

    #[tokio::test]
    async fn hard_fresh_restart_omission_preserves_active_context_policy_and_resets_accounting() {
        // Break caught: omitting the policy on an authorized fresh restart
        // preserves the incumbent limit while still resetting elapsed state.
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "fresh-omission".into(),
                pane: Some("%definitely-not-a-live-managed-pane".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    fresh_context_after_active_secs: Some(60),
                    active_context_accumulated_secs: 61,
                    active_context_segment_started_at: Some(100),
                    active_context_restart_due: true,
                    ..Default::default()
                },
            })
            .await;

        let (message, _, outcome) = restart_session_with_prompt_controls(
            &state,
            "fresh-omission",
            true,
            None,
            None,
            None,
            false,
            None,
            None,
            None,
            Some("claude-code"),
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
        )
        .await;

        assert_eq!(outcome, RestartOutcome::Restarted, "{message}");
        let protocol = state.protocol.read().await;
        let metadata = &protocol.sessions["fresh-omission"].metadata;
        assert_eq!(metadata.fresh_context_after_active_secs, Some(60));
        assert_eq!(metadata.active_context_accumulated_secs, 0);
        assert_eq!(metadata.active_context_segment_started_at, None);
        assert!(!metadata.active_context_restart_due);
        assert!(!metadata.active_context_accounting_provisional);
    }

    #[tokio::test]
    async fn hard_fresh_promptless_one_shot_applies_active_context_policy_without_storing_prompt() {
        // Break caught: a one-shot-only continuation must not become the
        // stored base prompt while fresh-policy completion still resets state.
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "promptless-one-shot".into(),
                pane: Some("%definitely-not-a-live-managed-pane".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    fresh_context_after_active_secs: Some(60),
                    active_context_accumulated_secs: 61,
                    active_context_restart_due: true,
                    ..Default::default()
                },
            })
            .await;

        let (message, _, outcome) = restart_session_with_prompt_controls(
            &state,
            "promptless-one-shot",
            true,
            Some(120),
            None,
            None,
            false,
            Some("one-shot continuation"),
            None,
            None,
            Some("claude-code"),
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
        )
        .await;

        assert_eq!(outcome, RestartOutcome::Restarted, "{message}");
        let protocol = state.protocol.read().await;
        let metadata = &protocol.sessions["promptless-one-shot"].metadata;
        assert_eq!(metadata.fresh_context_after_active_secs, Some(120));
        assert_eq!(metadata.active_context_accumulated_secs, 0);
        assert!(!metadata.active_context_restart_due);
        assert!(!metadata.active_context_accounting_provisional);
        assert_eq!(metadata.prompt, None);
    }

    #[tokio::test]
    async fn hard_restart_preserves_staged_hook_activity_and_due_boundary() {
        // Break caught: the production hard-restart path must not expose a
        // staged owner whose real Active/Stopped hooks have no receiver, nor
        // replace that receiver at completion.
        use axum::Json;
        use axum::extract::State as AxumState;

        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "hard-staged".into(),
                pane: Some("%old".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    fresh_context_after_active_secs: Some(1),
                    ..Default::default()
                },
            })
            .await;
        let control = crate::state::RestartTestControl::new(
            crate::state::RestartTestCheckpoint::HardBeforeCompletion,
        );
        state.set_restart_test_control(control.clone());
        let restart_state = state.clone();
        let restart = tokio::spawn(async move {
            restart_session_with_prompt_controls(
                &restart_state,
                "hard-staged",
                true,
                Some(1),
                None,
                None,
                false,
                None,
                None,
                None,
                Some("claude-code"),
                None,
                None,
                None,
                ParentSessionOverride::PreservePrevious,
                None,
            )
            .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            control.reached.notified(),
        )
        .await
        .expect("hard restart did not reach its test checkpoint within 2 seconds");

        let (target, pane) = {
            let protocol = state.protocol.read().await;
            let target = protocol.sessions["hard-staged"].owner();
            let lease = &protocol.lifecycle_leases["hard-staged"];
            (
                target,
                lease
                    .inert_pane
                    .clone()
                    .expect("hard restart must publish its fallback pane"),
            )
        };
        let _ = crate::hooks::prompt_submit(
            AxumState(state.clone()),
            Json(crate::hooks::PaneBody {
                pane: Some(pane.clone()),
                backend_session_id: None,
                session_incarnation: Some(target.incarnation),
            }),
        )
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if state.protocol.read().await.sessions["hard-staged"]
                    .metadata
                    .active_context_segment_started_at
                    .is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("staged hard Active hook must reach the target receiver");
        state
            .protocol
            .write()
            .await
            .sessions
            .get_mut("hard-staged")
            .expect("staged hard owner must remain current")
            .metadata
            .active_context_segment_started_at = Some(chrono::Utc::now().timestamp() - 2);
        let _ = crate::hooks::hook_stop(
            AxumState(state.clone()),
            Json(crate::hooks::PaneBody {
                pane: Some(pane),
                backend_session_id: None,
                session_incarnation: Some(target.incarnation),
            }),
        )
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if state.protocol.read().await.sessions["hard-staged"]
                    .metadata
                    .active_context_restart_due
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("staged hard Stopped hook must record the due boundary");
        assert!(
            state
                .try_set_pending_compact_continuation("hard-staged", "hard staged mailbox".into())
                .await
        );

        control.release.notify_one();
        let (message, _, outcome) =
            tokio::time::timeout(std::time::Duration::from_secs(5), restart)
                .await
                .expect("hard restart did not finish within 5 seconds")
                .expect("hard restart task failed");
        assert_eq!(outcome, RestartOutcome::Restarted, "{message}");
        let protocol = state.protocol.read().await;
        let metadata = &protocol.sessions["hard-staged"].metadata;
        assert_eq!(protocol.sessions["hard-staged"].owner(), target);
        assert!(metadata.active_context_accumulated_secs >= 2);
        assert!(metadata.active_context_restart_due);
        assert!(!metadata.active_context_accounting_provisional);
        drop(protocol);
        assert_eq!(
            state.drain_agent_compact_continuation_owned(&target).await,
            Some("hard staged mailbox".into())
        );
    }

    #[tokio::test]
    async fn rolled_back_fresh_completion_preserves_active_context_policy_and_accounting() {
        // Break caught: a stale success finalizer after rollback must not apply
        // the requested policy or reset the restored incumbent's accounting.
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "rolled-back".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    fresh_context_after_active_secs: Some(60),
                    active_context_accumulated_secs: 61,
                    active_context_segment_started_at: Some(100),
                    active_context_restart_due: true,
                    ..Default::default()
                },
            })
            .await;
        let lease_owner = state.protocol.read().await.sessions["rolled-back"].owner();
        assert_eq!(
            state.claim_existing_start(&lease_owner).await.unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        let target_owner = match state
            .stage_restart_launch(
                &lease_owner,
                "claude-code".into(),
                true,
                true,
                Some(120),
                None,
                None,
            )
            .await
        {
            crate::daemon_protocol::StageFreshLaunchOutcome::Staged { incarnation } => {
                crate::daemon_protocol::ResourceOwner {
                    session_id: "rolled-back".into(),
                    incarnation,
                }
            }
            other => panic!("expected staged restart, got {other:?}"),
        };
        assert_eq!(
            state
                .rollback_restart_launch(&lease_owner, &target_owner, None)
                .await
                .unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );

        let stale_outcome = state
            .complete_requested_restart_launch(
                &lease_owner,
                &target_owner,
                None,
                crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    fresh_context_after_active_secs: Some(120),
                    ..Default::default()
                },
                false,
                true,
            )
            .await
            .unwrap();

        assert_eq!(
            stale_outcome,
            crate::daemon_protocol::LifecycleMutationOutcome::NotFound
        );
        let protocol = state.protocol.read().await;
        let metadata = &protocol.sessions["rolled-back"].metadata;
        assert_eq!(metadata.fresh_context_after_active_secs, Some(60));
        assert_eq!(metadata.active_context_accumulated_secs, 61);
        assert_eq!(metadata.active_context_segment_started_at, Some(100));
        assert!(metadata.active_context_restart_due);
        assert!(!metadata.active_context_accounting_provisional);
    }

    #[tokio::test]
    async fn failed_opencode_recreate_restores_incumbent() {
        use axum::Json;
        use axum::Router;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::net::TcpListener;

        async fn health(State(reached): State<Arc<AtomicBool>>) -> Json<serde_json::Value> {
            reached.store(true, Ordering::SeqCst);
            Json(serde_json::json!({}))
        }

        async fn fail_create() -> StatusCode {
            StatusCode::BAD_GATEWAY
        }

        let setup_reached = Arc::new(AtomicBool::new(false));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/global/health", get(health))
            .route("/session", post(fail_create))
            .with_state(setup_reached.clone());
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
        let state = AppState::new(config);
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "same-id".into(),
                pane: Some("%definitely-not-a-live-managed-pane".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some(dir.path().to_string_lossy().into_owned()),
                    backend: Some("opencode".into()),
                    fresh_context_after_active_secs: Some(60),
                    active_context_accumulated_secs: 61,
                    active_context_segment_started_at: Some(100),
                    active_context_restart_due: true,
                    ..Default::default()
                },
            })
            .await;
        let previous = state.protocol.read().await.sessions["same-id"].clone();
        let owner = match reserve_start_for_launch(&state, "same-id").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Existing(owner) => owner,
            other => panic!("expected existing owner, got {other:?}"),
        };
        assert_eq!(
            state.claim_existing_start(&owner).await.unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );

        let (_, _, outcome) = restart_session_for_start_with_active_context_policy(
            &state,
            &owner,
            "same-id",
            true,
            Some(120),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
        )
        .await;

        assert_eq!(outcome, RestartOutcome::Failed);
        assert!(
            setup_reached.load(Ordering::SeqCst),
            "the staged restart owner must authorize shared-serve setup"
        );
        let proto = state.protocol.read().await;
        assert_eq!(proto.sessions["same-id"], previous);
        let metadata = &proto.sessions["same-id"].metadata;
        assert_eq!(metadata.fresh_context_after_active_secs, Some(60));
        assert_eq!(metadata.active_context_accumulated_secs, 61);
        assert_eq!(metadata.active_context_segment_started_at, Some(100));
        assert!(metadata.active_context_restart_due);
        assert!(!metadata.active_context_accounting_provisional);
        assert!(proto.lifecycle_leases.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn failed_respawn_and_fallback_creation_restore_the_staged_identity() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "restart".into(),
                pane: Some("%original".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("thread-old".into()),
                    ..Default::default()
                },
            })
            .await;
        let previous = state.protocol.read().await.sessions["restart"].clone();
        let crate::daemon_protocol::StageFreshLaunchOutcome::Staged {
            incarnation: staged_incarnation,
        } = state
            .stage_fresh_launch("restart", "codex-cli".into(), Some("proof".into()), None)
            .await
        else {
            panic!("stage must be accepted");
        };

        recover_failed_fresh_launch(
            &state,
            "restart",
            Some("%original".into()),
            Some("proof".into()),
            Some(staged_incarnation),
            Some(previous.clone()),
            None,
        )
        .await
        .unwrap();

        assert_eq!(state.protocol.read().await.sessions["restart"], previous);
    }

    #[tokio::test]
    async fn failed_fallback_send_keys_restores_paneless_stage_and_cleans_fallback() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "restart".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    backend_session_id: Some("thread-old".into()),
                    ..Default::default()
                },
            })
            .await;
        let previous = state.protocol.read().await.sessions["restart"].clone();
        let crate::daemon_protocol::StageFreshLaunchOutcome::Staged {
            incarnation: staged_incarnation,
        } = state
            .stage_fresh_launch("restart", "claude-code".into(), None, None)
            .await
        else {
            panic!("stage must be accepted");
        };

        recover_failed_fresh_launch(
            &state,
            "restart",
            None,
            None,
            Some(staged_incarnation),
            Some(previous.clone()),
            Some("%fallback".into()),
        )
        .await
        .unwrap();

        assert_eq!(state.protocol.read().await.sessions["restart"], previous);
    }

    #[tokio::test]
    async fn failed_fallback_launch_restores_previous_owner_and_clears_inert_record() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "restart".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    backend_session_id: Some("thread-old".into()),
                    ..Default::default()
                },
            })
            .await;
        let previous = state.protocol.read().await.sessions["restart"].clone();
        let lease_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "restart".into(),
            incarnation: previous.metadata.session_incarnation,
        };
        assert_eq!(
            state.claim_existing_start(&lease_owner).await.unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        let crate::daemon_protocol::StageFreshLaunchOutcome::Staged {
            incarnation: staged_incarnation,
        } = state
            .stage_fresh_launch("restart", "claude-code".into(), None, None)
            .await
        else {
            panic!("stage must be accepted");
        };
        let pane_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "restart".into(),
            incarnation: staged_incarnation,
        };
        assert_eq!(
            state
                .record_inert_start_pane(&lease_owner, pane_owner.clone(), "%fallback".into(),)
                .await
                .unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        let staged_metadata = state.protocol.read().await.sessions["restart"]
            .metadata
            .clone();
        assert_eq!(
            state
                .finalize_reserved_start(&pane_owner, Some("%fallback".into()), staged_metadata,)
                .await
                .unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );

        recover_failed_fresh_launch(
            &state,
            "restart",
            Some("%fallback".into()),
            None,
            Some(staged_incarnation),
            Some(previous.clone()),
            Some("%fallback".into()),
        )
        .await
        .unwrap();

        let proto = state.protocol.read().await;
        assert_eq!(proto.sessions["restart"], previous);
        assert_eq!(proto.lifecycle_leases["restart"].owner, lease_owner);
        assert!(proto.lifecycle_leases["restart"].inert_pane.is_none());
        assert!(proto.lifecycle_leases["restart"].inert_pane_owner.is_none());
    }

    #[tokio::test]
    async fn fallback_creation_failure_without_a_stage_is_a_noop() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "restart".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend_session_id: Some("thread-old".into()),
                    ..Default::default()
                },
            })
            .await;
        let previous = state.protocol.read().await.sessions["restart"].clone();

        recover_failed_fresh_launch(
            &state,
            "restart",
            None,
            None,
            None,
            Some(previous.clone()),
            None,
        )
        .await
        .unwrap();

        assert_eq!(state.protocol.read().await.sessions["restart"], previous);
    }

    #[tokio::test]
    async fn failed_fallback_launch_preserves_a_concurrent_session_start() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "restart".into(),
                pane: Some("%original".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("thread-old".into()),
                    ..Default::default()
                },
            })
            .await;
        let previous = state.protocol.read().await.sessions["restart"].clone();
        let stage_outcome = state
            .stage_fresh_launch("restart", "codex-cli".into(), Some("proof".into()), None)
            .await;
        assert!(matches!(
            stage_outcome,
            crate::daemon_protocol::StageFreshLaunchOutcome::Staged { .. }
        ));
        let staged_metadata = state.protocol.read().await.sessions["restart"]
            .metadata
            .clone();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "restart".into(),
                pane: Some("%fallback".into()),
                metadata: staged_metadata,
            })
            .await;
        let staged_incarnation = state.protocol.read().await.sessions["restart"]
            .metadata
            .session_incarnation;
        state
            .apply_and_execute(crate::daemon_protocol::Event::AdoptBackend {
                id: "restart".into(),
                backend: "codex-cli".into(),
                backend_session_id: "thread-winner".into(),
                expected_backend_session_id: None,
                expected_session_start_credential: Some("proof".into()),
            })
            .await;

        recover_failed_fresh_launch(
            &state,
            "restart",
            Some("%fallback".into()),
            Some("proof".into()),
            Some(staged_incarnation),
            Some(previous),
            Some("%fallback".into()),
        )
        .await
        .unwrap();

        let retained = state.protocol.read().await.sessions["restart"].clone();
        assert_eq!(retained.pane.as_deref(), Some("%fallback"));
        assert_eq!(
            retained.metadata.backend_session_id.as_deref(),
            Some("thread-winner")
        );
    }

    #[tokio::test]
    async fn cleanup_provisional_start_only_removes_its_registered_pane() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "codex-start".into(),
                pane: Some("%created".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;

        cleanup_provisional_start(&state, "codex-start", "%other").await;
        assert!(
            state
                .protocol
                .read()
                .await
                .sessions
                .contains_key("codex-start")
        );

        cleanup_provisional_start(&state, "codex-start", "%created").await;
        assert!(
            !state
                .protocol
                .read()
                .await
                .sessions
                .contains_key("codex-start")
        );
    }

    async fn wait_for_prompt_fallback_timer() {
        tokio::time::sleep(PENDING_PROMPT_FALLBACK_DELAY + std::time::Duration::from_millis(10))
            .await;
        tokio::task::yield_now().await;
    }

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
    fn secret_is_ephemeral_and_unique() {
        let s1 = generate_secret();
        let s2 = generate_secret();
        assert_eq!(s1.len(), 32);
        assert_eq!(s2.len(), 32);
        assert!(s1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(s1, s2, "each generated secret must be unique");
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
    fn parse_command_connect() {
        match parse_human_command("/connect nprofile1abc") {
            HumanCommand::Command(cmd) => assert_eq!(cmd, "/connect nprofile1abc"),
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn parse_command_nodes() {
        assert!(matches!(
            parse_human_command("/nodes"),
            HumanCommand::Command(_)
        ));
    }

    #[test]
    fn parse_command_task() {
        assert!(matches!(
            parse_human_command("/task list"),
            HumanCommand::Command(_)
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

    // --- opencode_prompt_body ---

    #[test]
    fn opencode_prompt_body_text_only() {
        let body = opencode_prompt_body("hello", None, None);
        assert_eq!(
            body,
            serde_json::json!({
                "parts": [{"type": "text", "text": "hello"}]
            })
        );
    }

    #[test]
    fn opencode_attach_command_shell_escapes_project_dir() {
        let cmd = opencode_attach_command(8200, "ses_test", "/tmp/project with spaces");
        assert_eq!(
            cmd,
            "opencode attach http://127.0.0.1:8200 --session 'ses_test' --dir '/tmp/project with spaces'"
        );
    }

    #[test]
    fn opencode_attach_skew_notice_command_shell_escapes_versions() {
        // Version strings flow in from the serve's /global/health body and
        // `opencode --version` stdout; a stray quote must not break out of the
        // notice command and inject shell (matches opencode_attach_command).
        let cmd = opencode_attach_skew_notice_command("1.14.31'; touch PWNED; #", "1.17.7", 7880);
        // The malicious serve version is single-quoted as one escaped token, so
        // the injected `; touch PWNED` stays inside a quoted string.
        assert!(
            cmd.contains("'1.14.31'\\''; touch PWNED; #'"),
            "serve version not shell-escaped: {cmd}"
        );
        assert!(cmd.contains("'1.17.7'"), "client version not quoted: {cmd}");
        // No unescaped injection: the only `touch PWNED` occurrence sits inside
        // the escaped token above, never as a bare command.
        assert!(
            !cmd.contains("31; touch"),
            "injection escaped the quoting: {cmd}"
        );
    }

    #[test]
    fn opencode_attach_versions_compatible_matches_exact_version() {
        assert!(opencode_attach_versions_compatible("1.14.31", "1.14.31"));
        // Surrounding whitespace from `--version` output is ignored.
        assert!(opencode_attach_versions_compatible("1.14.31", " 1.14.31\n"));
    }

    #[test]
    fn opencode_attach_versions_compatible_rejects_skew() {
        // The crash repro: serve 1.14.31, attach client 1.17.7.
        assert!(!opencode_attach_versions_compatible("1.14.31", "1.17.7"));
        assert!(!opencode_attach_versions_compatible("1.14.31", ""));
    }

    #[test]
    fn prompt_async_fallback_uses_raw_tmux_delivery() {
        assert_eq!(prompt_fallback_delivery(), PromptFallbackDelivery::RawTmux);
    }

    #[tokio::test]
    async fn route_human_message_marks_failed_http_delivery_undelivered() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "target".into(),
                pane: Some("%target".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_target".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;

        route_human_message(&state, "human", "target", "hello").await;

        let log = state.message_log.read().await;
        let entry = log.back().expect("human DM should be logged");
        assert_eq!(entry.from, "human");
        assert_eq!(entry.to, "target");
        assert!(!entry.delivered, "failed HTTP delivery must be observable");
    }

    #[test]
    fn prompt_fallback_requires_live_opencode_tui_for_http_api() {
        assert!(!should_deliver_prompt_fallback(true, false));
        assert!(should_deliver_prompt_fallback(true, true));
        assert!(should_deliver_prompt_fallback(false, false));
    }

    #[test]
    fn prompt_async_fallback_classifier_rejects_ambiguous_server_errors() {
        assert_eq!(
            classify_prompt_async_fallback(PromptAsyncFailure::Status(
                reqwest::StatusCode::BAD_GATEWAY
            )),
            PromptAsyncFallbackDecision::Ambiguous
        );
        assert_eq!(
            classify_prompt_async_fallback(PromptAsyncFailure::Status(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR
            )),
            PromptAsyncFallbackDecision::Ambiguous
        );
    }

    #[test]
    fn prompt_async_fallback_classifier_allows_known_not_accepted_statuses() {
        assert_eq!(
            classify_prompt_async_fallback(PromptAsyncFailure::Status(
                reqwest::StatusCode::NOT_FOUND
            )),
            PromptAsyncFallbackDecision::DefiniteNonAcceptance
        );
        assert_eq!(
            classify_prompt_async_fallback(PromptAsyncFailure::Status(
                reqwest::StatusCode::BAD_REQUEST
            )),
            PromptAsyncFallbackDecision::DefiniteNonAcceptance
        );
    }

    #[tokio::test]
    async fn prompt_async_fallback_classifier_allows_connection_errors() {
        let error = reqwest::Client::new()
            .post("http://[::1]:1/session/ses/prompt_async")
            .send()
            .await
            .unwrap_err();

        assert!(error.is_connect());
        assert_eq!(
            classify_prompt_async_fallback(PromptAsyncFailure::Request(&error)),
            PromptAsyncFallbackDecision::DefiniteNonAcceptance
        );
    }

    #[tokio::test]
    async fn prompt_async_fallback_classifier_rejects_timeout_errors() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;

        async fn prompt_async() -> StatusCode {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            StatusCode::NO_CONTENT
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/session/{session_id}/prompt_async", post(prompt_async));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let error = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/session/ses/prompt_async"))
            .timeout(std::time::Duration::from_millis(1))
            .send()
            .await
            .unwrap_err();

        assert!(error.is_timeout());
        assert_eq!(
            classify_prompt_async_fallback(PromptAsyncFailure::Request(&error)),
            PromptAsyncFallbackDecision::Ambiguous
        );
        server.abort();
    }

    #[tokio::test]
    async fn prompt_fallback_uses_recorded_http_api_policy_for_missing_session() {
        let state = AppState::new_for_test();

        let result = deliver_prompt_fallback(
            &state, "missing", "%missing", "hello", true, false, None, None,
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn prompt_fallback_rejects_pane_no_longer_registered_to_session() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%current".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;

        let result =
            deliver_prompt_fallback(&state, "oc", "%stale", "hello", false, false, None, None)
                .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn prompt_fallback_rejects_stale_opencode_backend_session() {
        let state = AppState::new_for_test();
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

        let result = deliver_prompt_fallback(
            &state,
            "oc",
            "%17",
            "hello",
            false,
            false,
            Some("ses_old"),
            None,
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn readiness_timeout_keeps_pending_prompt_when_raw_fallback_fails() {
        let state = AppState::new_for_test();
        schedule_prompt_injection(
            &state,
            "oc",
            "%missing".into(),
            "queued prompt".into(),
            None,
        );

        wait_for_prompt_fallback_timer().await;

        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&crate::state::PendingPrompt::new(
                "%missing".into(),
                "queued prompt".into(),
                None,
            ))
        );
    }

    #[tokio::test]
    async fn readiness_timeout_discards_prompt_for_superseded_restart_target() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%old".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_same".into()),
                    ..Default::default()
                },
            })
            .await;
        let old_owner = state.protocol.read().await.sessions["oc"].owner();
        schedule_prompt_injection_for_owner(
            &state,
            "oc",
            "%old".into(),
            "stale restart prompt".into(),
            Some("ses_same".into()),
            Some(old_owner),
        );
        {
            let mut proto = state.protocol.write().await;
            proto
                .sessions
                .get_mut("oc")
                .unwrap()
                .metadata
                .session_incarnation = crate::daemon_protocol::SessionIncarnation(999);
        }

        wait_for_prompt_fallback_timer().await;

        assert!(!state.pending_prompts.lock().unwrap().contains_key("oc"));
    }

    #[tokio::test]
    async fn pending_prompt_fallback_retry_consumes_restored_prompt() {
        let state = AppState::new_for_test();
        let pending =
            crate::state::PendingPrompt::new("%eventual".into(), "queued prompt".into(), None);
        restore_pending_prompt_if_absent(&state, "oc", pending.clone());

        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%eventual".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        schedule_pending_prompt_fallback_retry(&state, "oc", pending, false);
        wait_for_prompt_fallback_timer().await;

        assert!(state.pending_prompts.lock().unwrap().get("oc").is_none());
    }

    #[tokio::test]
    async fn start_prompt_fallback_failure_restores_pending_prompt() {
        let state = AppState::new_for_test();
        let pending = crate::state::PendingPrompt::new(
            "%eventual".into(),
            "queued prompt".into(),
            Some("ses_oc".into()),
        );

        restore_start_prompt_after_fallback_failure(&state, "oc", pending.clone());
        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending)
        );

        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%eventual".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        wait_for_prompt_fallback_timer().await;

        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending)
        );
    }

    #[tokio::test]
    async fn restart_prompt_fallback_failure_restores_pending_prompt() {
        let state = AppState::new_for_test();
        let pending = crate::state::PendingPrompt::new(
            "%eventual".into(),
            "queued prompt".into(),
            Some("ses_oc".into()),
        );

        restore_restart_prompt_after_fallback_failure(&state, "oc", pending.clone());

        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending)
        );

        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%eventual".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        wait_for_prompt_fallback_timer().await;

        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending)
        );
    }

    #[test]
    fn readiness_timeout_reserves_prompt_before_raw_fallback() {
        let state = AppState::new_for_test();
        state.pending_prompts.lock().unwrap().insert(
            "oc".into(),
            crate::state::PendingPrompt::new("%pane".into(), "queued prompt".into(), None),
        );

        let reserved =
            reserve_pending_prompt_if_matches(&state, "oc", "%pane", "queued prompt", None);

        assert_eq!(
            reserved,
            Some(crate::state::PendingPrompt::new(
                "%pane".into(),
                "queued prompt".into(),
                None,
            ))
        );
        assert!(state.pending_prompts.lock().unwrap().get("oc").is_none());
    }

    #[test]
    fn start_prompt_is_unavailable_when_http_api_has_no_attached_session() {
        assert_eq!(
            start_prompt_delivery(true, None),
            StartPromptDelivery::Unavailable
        );
    }

    #[test]
    fn unavailable_start_prompt_does_not_expose_msg_id() {
        assert_eq!(
            start_prompt_msg_id(Some(42), Some(StartPromptDelivery::Unavailable)),
            None
        );
    }

    #[test]
    fn parent_session_override_no_parent_clears_previous_parent() {
        let previous = crate::daemon_protocol::SessionMeta {
            parent_session: Some("old-parent".into()),
            ..Default::default()
        };

        assert_eq!(
            ParentSessionOverride::PreservePrevious.resolve(Some(&previous)),
            Some("old-parent".into())
        );
        assert_eq!(
            ParentSessionOverride::SetParent("new-parent".into()).resolve(Some(&previous)),
            Some("new-parent".into())
        );
        assert_eq!(
            ParentSessionOverride::NoParent.resolve(Some(&previous)),
            None
        );
    }

    #[test]
    fn restart_prompt_resolution_has_one_persistent_base_and_launch_only_suffix() {
        struct Case {
            stored: Option<&'static str>,
            replacement: Option<&'static str>,
            suppress: bool,
            one_shot: Option<&'static str>,
            expected_launch: Option<&'static str>,
            expected_stored: Option<&'static str>,
        }

        let cases = [
            Case {
                stored: Some("stored"),
                replacement: None,
                suppress: false,
                one_shot: None,
                expected_launch: Some("stored"),
                expected_stored: Some("stored"),
            },
            Case {
                stored: Some("stored"),
                replacement: Some("replacement"),
                suppress: false,
                one_shot: None,
                expected_launch: Some("replacement"),
                expected_stored: Some("replacement"),
            },
            Case {
                stored: Some("stored"),
                replacement: None,
                suppress: true,
                one_shot: None,
                expected_launch: None,
                expected_stored: Some("stored"),
            },
            Case {
                stored: Some("stored"),
                replacement: None,
                suppress: false,
                one_shot: Some("launch only"),
                expected_launch: Some("stored\n\nlaunch only"),
                expected_stored: Some("stored"),
            },
            Case {
                stored: Some("stored"),
                replacement: None,
                suppress: true,
                one_shot: Some("launch only"),
                expected_launch: Some("launch only"),
                expected_stored: Some("stored"),
            },
            Case {
                stored: Some("stored"),
                replacement: Some("replacement"),
                suppress: true,
                one_shot: Some("launch only"),
                expected_launch: Some("replacement\n\nlaunch only"),
                expected_stored: Some("replacement"),
            },
            Case {
                stored: None,
                replacement: None,
                suppress: true,
                one_shot: Some("launch only"),
                expected_launch: Some("launch only"),
                expected_stored: None,
            },
        ];

        for case in cases {
            let resolved = resolve_restart_prompt(
                case.stored,
                RestartPromptInput {
                    replacement: case.replacement,
                    suppress_stored: case.suppress,
                    one_shot: case.one_shot,
                },
            );
            assert_eq!(resolved.launch.as_deref(), case.expected_launch);
            assert_eq!(resolved.stored.as_deref(), case.expected_stored);
        }
    }

    #[test]
    fn active_context_policy_selection_applies_only_to_initial_or_fresh_launches() {
        // Break caught: initial spawn and authorized fresh restart may apply a
        // request; omission and nonfresh recovery preserve the current limit.
        assert_eq!(
            active_context_policy_for_launch(None, Some(120), true),
            Some(120)
        );
        assert_eq!(
            active_context_policy_for_launch(Some(60), Some(120), true),
            Some(120)
        );
        assert_eq!(
            active_context_policy_for_launch(Some(60), None, true),
            Some(60)
        );
        assert_eq!(
            active_context_policy_for_launch(Some(60), Some(120), false),
            Some(60)
        );
    }

    #[cfg(unix)]
    #[test]
    fn tui_launch_prompt_files_are_unique_private_and_drop_cleaned() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let first =
            prepare_tui_launch_command_in(dir.path(), "backend", Some("first prompt")).unwrap();
        let second =
            prepare_tui_launch_command_in(dir.path(), "backend", Some("second prompt")).unwrap();
        let first_path = first.prompt_path().unwrap().to_path_buf();
        let second_path = second.prompt_path().unwrap().to_path_buf();

        assert_ne!(first_path, second_path);
        assert_eq!(
            std::fs::metadata(&first_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&second_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        drop(first);
        drop(second);
        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }

    #[test]
    fn tui_launch_prompt_preparation_propagates_creation_errors_without_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_directory = dir.path().join("plain-file");
        std::fs::write(&not_a_directory, "occupied").unwrap();

        let error =
            prepare_tui_launch_command_in(&not_a_directory, "backend", Some("secret")).unwrap_err();

        assert!(error.to_string().contains("launch prompt"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn tui_launch_prompt_preparation_cleans_partial_file_after_write_error() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let error = prepare_tui_launch_command_in_with_writer(
            dir.path(),
            "backend",
            "secret",
            |file, _| {
                file.write_all(b"partial")?;
                Err(std::io::Error::other("injected write failure"))
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("write private launch prompt"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn tui_launch_prompt_rejects_non_utf8_temp_path_without_leaking_file() {
        use std::os::unix::ffi::OsStringExt;

        let root = tempfile::tempdir().unwrap();
        let non_utf8 = root
            .path()
            .join(std::ffi::OsString::from_vec(b"non-utf8-\xff".to_vec()));
        std::fs::create_dir(&non_utf8).unwrap();

        let error =
            prepare_tui_launch_command_in(&non_utf8, "backend", Some("secret")).unwrap_err();

        assert!(error.to_string().contains("UTF-8"));
        assert_eq!(std::fs::read_dir(&non_utf8).unwrap().count(), 0);
    }

    #[test]
    fn failed_absent_and_existing_tui_launch_guards_leave_no_prompt_file() {
        let dir = tempfile::tempdir().unwrap();

        for path_kind in ["absent-session start", "existing-session restart"] {
            let prepared =
                prepare_tui_launch_command_in(dir.path(), "backend", Some(path_kind)).unwrap();
            let prompt_path = prepared.prompt_path().unwrap().to_path_buf();
            assert!(prompt_path.exists());

            // A failed tmux launch never hands ownership to the shell.
            drop(prepared);
            assert!(!prompt_path.exists(), "{path_kind} leaked its prompt file");
        }
    }

    #[test]
    fn handed_off_prompt_survives_arbitrary_delay_until_shell_consumes_it() {
        let dir = tempfile::tempdir().unwrap();

        for path_kind in ["absent-session start", "existing-session restart"] {
            let mut prepared =
                prepare_tui_launch_command_in(dir.path(), "printf '%s'", Some(path_kind)).unwrap();
            let prompt_path = prepared.prompt_path().unwrap().to_path_buf();
            let command = prepared.command().to_string();

            // Tmux acceptance is irreversible. No wall-clock cleanup may race
            // a shell that has accepted the command but has not read it yet.
            let _: () = prepared.mark_handed_off();
            drop(prepared);
            assert!(
                prompt_path.exists(),
                "{path_kind} lost its prompt before delayed consumption"
            );

            let output = std::process::Command::new("sh")
                .args(["-c", &command])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, path_kind.as_bytes());
            assert!(!prompt_path.exists());
        }
    }

    #[test]
    fn accepted_tui_launch_unlinks_prompt_before_backend_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut prepared =
            prepare_tui_launch_command_in(dir.path(), "printf '%s'", Some("secret")).unwrap();
        let prompt_path = prepared.prompt_path().unwrap().to_path_buf();

        let output = std::process::Command::new("sh")
            .args(["-c", prepared.command()])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"secret");
        assert!(!prompt_path.exists());

        // Mirrors a successful tmux handoff. It remains harmless if the shell
        // already consumed and unlinked the file before tmux returned.
        let _: () = prepared.mark_handed_off();
    }

    #[test]
    fn http_backend_launch_remains_file_free_and_does_not_embed_prompt() {
        let prepared =
            prepare_backend_launch_command(true, "opencode serve", Some("launch only")).unwrap();

        assert_eq!(prepared.command(), "opencode serve");
        assert!(prepared.prompt_path().is_none());
    }

    #[tokio::test]
    async fn soft_restart_metadata_persists_replacement_but_not_one_shot_suffix() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    prompt: Some("stored".into()),
                    ..Default::default()
                },
            })
            .await;
        let owner = {
            let proto = state.protocol.read().await;
            SoftRestartOwnerSnapshot {
                session_id: "oc".into(),
                incarnation: proto.sessions["oc"].metadata.session_incarnation,
            }
        };

        apply_soft_restart_metadata(
            &state,
            &owner,
            "ses_new",
            0,
            SoftRestartMetadataUpdate {
                prompt_replacement: Some("replacement"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions["oc"].metadata.prompt.as_deref(),
            Some("replacement")
        );
        assert_ne!(
            proto.sessions["oc"].metadata.prompt.as_deref(),
            Some("replacement\n\nlaunch only")
        );
    }

    #[test]
    fn restart_reusing_previous_backend_session_preserves_weak_opencode_binding() {
        assert_eq!(
            opencode_binding_for_restart_session(
                true,
                Some("previous-session"),
                true,
                Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted),
            ),
            Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted)
        );
    }

    #[test]
    fn restart_reusing_previous_backend_session_without_binding_defaults_weak() {
        assert_eq!(
            opencode_binding_for_restart_session(true, Some("previous-session"), true, None),
            Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted)
        );
    }

    #[test]
    fn non_fresh_backend_switch_never_reuses_previous_native_identity() {
        let previous = crate::daemon_protocol::SessionMeta {
            backend: Some("opencode".into()),
            backend_session_id: Some("ses_opencode".into()),
            ..Default::default()
        };

        assert_eq!(
            select_restart_resume_id(
                false,
                "codex-cli",
                Some(&previous),
                Some("thread_detected".into()),
            ),
            None,
            "neither the stored OpenCode ID nor a detected Codex thread may cross a backend switch"
        );
        assert_eq!(
            select_restart_resume_id(false, "opencode", Some(&previous), None),
            Some("ses_opencode".into())
        );
    }

    #[test]
    fn hard_opencode_fallback_never_reuses_cross_backend_native_identity() {
        let previous = crate::daemon_protocol::SessionMeta {
            backend: Some("codex-cli".into()),
            backend_session_id: Some("thread_codex".into()),
            ..Default::default()
        };

        assert_eq!(
            previous_http_restart_fallback_id(false, "opencode", Some(&previous)),
            None,
            "a reachable Codex thread ID must never be probed or adopted as an OpenCode session"
        );
    }

    #[test]
    fn non_fresh_http_restart_prefers_stored_backend_session() {
        let previous = crate::daemon_protocol::SessionMeta {
            backend: Some("opencode".into()),
            backend_session_id: Some("ses_preserved".into()),
            ..Default::default()
        };

        assert_eq!(
            select_http_restart_backend_plan(true, false, "opencode", Some(&previous)),
            Some(HttpRestartBackendPlan::Reuse("ses_preserved".into()))
        );
    }

    #[test]
    fn fresh_http_restart_creates_new_backend_session() {
        let previous = crate::daemon_protocol::SessionMeta {
            backend: Some("opencode".into()),
            backend_session_id: Some("ses_previous".into()),
            ..Default::default()
        };

        assert_eq!(
            select_http_restart_backend_plan(true, true, "opencode", Some(&previous)),
            Some(HttpRestartBackendPlan::Create)
        );
    }

    #[test]
    fn non_http_restart_has_no_opencode_backend_plan() {
        let previous = crate::daemon_protocol::SessionMeta {
            backend: Some("codex-cli".into()),
            backend_session_id: Some("thread_previous".into()),
            ..Default::default()
        };

        assert_eq!(
            select_http_restart_backend_plan(false, false, "codex-cli", Some(&previous)),
            None
        );
    }

    #[test]
    fn restart_final_refresh_preserves_selected_codex_resume_id() {
        assert_eq!(
            final_restart_backend_binding(
                "codex-cli",
                Some("thread-resumed".into()),
                None,
                None,
                None,
            ),
            (Some("thread-resumed".into()), None),
            "the thread ID used by `codex resume` must survive the TUI metadata refresh"
        );
    }

    #[test]
    fn restart_final_refresh_preserves_session_start_that_arrived_before_refresh() {
        assert_eq!(
            final_restart_backend_binding(
                "codex-cli",
                None,
                Some("launch-credential".into()),
                None,
                Some((Some("thread-bound-early".into()), None)),
            ),
            (Some("thread-bound-early".into()), None),
            "a SessionStart that consumes the credential before the final refresh remains bound"
        );
    }

    #[test]
    fn restart_final_refresh_leaves_credential_for_session_start_after_refresh() {
        assert_eq!(
            final_restart_backend_binding(
                "codex-cli",
                None,
                Some("launch-credential".into()),
                None,
                None,
            ),
            (None, Some("launch-credential".into())),
            "a SessionStart that arrives after the final refresh still receives its pending credential"
        );
    }

    #[test]
    fn start_registration_skips_http_api_placeholder_without_backend_session() {
        assert!(start_registration_metadata(true, "%1", None).is_none());
    }

    #[test]
    fn failed_start_placeholder_cleanup_required_without_backend_session() {
        assert!(should_cleanup_failed_opencode_attach_pane(true, None));
    }

    fn incumbent_test_owners() -> (
        crate::daemon_protocol::ResourceOwner,
        crate::daemon_protocol::ResourceOwner,
    ) {
        (
            crate::daemon_protocol::ResourceOwner {
                session_id: "worker".into(),
                incarnation: crate::daemon_protocol::SessionIncarnation(10),
            },
            crate::daemon_protocol::ResourceOwner {
                session_id: "worker".into(),
                incarnation: crate::daemon_protocol::SessionIncarnation(11),
            },
        )
    }

    #[test]
    fn missing_incumbent_pane_is_recreated() {
        let (lease_owner, restart_target_owner) = incumbent_test_owners();

        assert_eq!(
            classify_incumbent_pane(
                &crate::tmux::ManagedPaneInspection::Missing,
                &lease_owner,
                &restart_target_owner,
            ),
            IncumbentPaneDisposition::Recreate
        );
    }

    #[test]
    fn unmanaged_incumbent_pane_is_refused() {
        let (lease_owner, restart_target_owner) = incumbent_test_owners();

        assert_eq!(
            classify_incumbent_pane(
                &crate::tmux::ManagedPaneInspection::Unmanaged,
                &lease_owner,
                &restart_target_owner,
            ),
            IncumbentPaneDisposition::Refuse
        );
    }

    #[test]
    fn process_lease_owned_incumbent_pane_is_respawned() {
        let (lease_owner, restart_target_owner) = incumbent_test_owners();

        assert_eq!(
            classify_incumbent_pane(
                &crate::tmux::ManagedPaneInspection::ProcessOwner(lease_owner.clone()),
                &lease_owner,
                &restart_target_owner,
            ),
            IncumbentPaneDisposition::Respawn
        );
    }

    #[test]
    fn marker_lease_owned_incumbent_pane_is_respawned() {
        let (lease_owner, restart_target_owner) = incumbent_test_owners();

        assert_eq!(
            classify_incumbent_pane(
                &crate::tmux::ManagedPaneInspection::MarkerOwner(lease_owner.clone()),
                &lease_owner,
                &restart_target_owner,
            ),
            IncumbentPaneDisposition::Respawn
        );
    }

    #[test]
    fn process_restart_target_owned_incumbent_pane_is_respawned() {
        let (lease_owner, restart_target_owner) = incumbent_test_owners();

        assert_eq!(
            classify_incumbent_pane(
                &crate::tmux::ManagedPaneInspection::ProcessOwner(restart_target_owner.clone(),),
                &lease_owner,
                &restart_target_owner,
            ),
            IncumbentPaneDisposition::Respawn
        );
    }

    #[test]
    fn marker_restart_target_owned_incumbent_pane_is_respawned() {
        let (lease_owner, restart_target_owner) = incumbent_test_owners();

        assert_eq!(
            classify_incumbent_pane(
                &crate::tmux::ManagedPaneInspection::MarkerOwner(restart_target_owner.clone(),),
                &lease_owner,
                &restart_target_owner,
            ),
            IncumbentPaneDisposition::Respawn
        );
    }

    #[test]
    fn stranger_process_owned_incumbent_pane_is_refused() {
        let (lease_owner, restart_target_owner) = incumbent_test_owners();
        let stranger_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "stranger".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(12),
        };

        assert_eq!(
            classify_incumbent_pane(
                &crate::tmux::ManagedPaneInspection::ProcessOwner(stranger_owner),
                &lease_owner,
                &restart_target_owner,
            ),
            IncumbentPaneDisposition::Refuse
        );
    }

    #[test]
    fn stranger_marker_owned_incumbent_pane_is_refused() {
        let (lease_owner, restart_target_owner) = incumbent_test_owners();
        let stranger_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "stranger".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(12),
        };

        assert_eq!(
            classify_incumbent_pane(
                &crate::tmux::ManagedPaneInspection::MarkerOwner(stranger_owner),
                &lease_owner,
                &restart_target_owner,
            ),
            IncumbentPaneDisposition::Refuse
        );
    }

    #[test]
    fn shared_serve_session_requires_verified_attach() {
        let result = shared_serve_session_after_attach("ses_123".to_string(), false, "%1");
        assert!(result.is_err());
    }

    #[test]
    fn created_opencode_session_id_rejects_url_delimiters() {
        for session_id in ["bad/id", "bad?id", "bad#id", "bad id"] {
            let error = validate_created_opencode_session_id(session_id)
                .expect_err("invalid created session id must be rejected");
            assert!(error.to_string().contains("invalid backend_session_id"));
        }

        assert_eq!(
            validate_created_opencode_session_id("ses_good-123").unwrap(),
            "ses_good-123"
        );
    }

    #[test]
    fn opencode_attach_command_shell_escapes_session_id() {
        let command = opencode_attach_command(7880, "abc; touch PWNED; #", "/tmp/project dir");

        assert_eq!(
            command,
            "opencode attach http://127.0.0.1:7880 --session 'abc; touch PWNED; #' --dir '/tmp/project dir'"
        );
    }

    #[tokio::test]
    async fn cleanup_created_opencode_session_sends_delete() {
        use axum::Router;
        use axum::extract::{Path, State};
        use axum::http::StatusCode;
        use axum::routing::delete;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::net::TcpListener;

        async fn delete_session(
            State(deleted): State<Arc<AtomicBool>>,
            Path(session_id): Path<String>,
        ) -> StatusCode {
            if session_id == "ses_leak" {
                deleted.store(true, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            } else {
                StatusCode::NOT_FOUND
            }
        }

        let deleted = Arc::new(AtomicBool::new(false));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}", delete(delete_session))
            .with_state(deleted.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let cleaned =
            delete_opencode_session(&reqwest::Client::new(), port, "ses_leak", "test").await;

        assert!(cleaned);
        assert!(deleted.load(Ordering::SeqCst));
        server.abort();
    }

    #[tokio::test]
    async fn opencode_serve_version_reads_health_body() {
        use axum::Json;
        use axum::Router;
        use axum::routing::get;
        use tokio::net::TcpListener;

        async fn health() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "version": "1.14.31" }))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/global/health", get(health));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let version = opencode_serve_version(&reqwest::Client::new(), port).await;
        assert_eq!(version.as_deref(), Some("1.14.31"));
        server.abort();
    }

    #[tokio::test]
    async fn opencode_serve_version_fails_open_on_non_success() {
        // A serve that is up but returns an error must yield None so the skew
        // guard fails open rather than blocking the attach.
        let version = opencode_serve_version(&reqwest::Client::new(), 1).await;
        assert!(version.is_none());
    }

    #[tokio::test]
    async fn soft_restart_marks_new_opencode_session_strong_managed() {
        use axum::Json;
        use axum::Router;
        use axum::routing::post;
        use tokio::net::TcpListener;

        async fn create_session() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "id": "ses_new" }))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/session", post(create_session));
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
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_old".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::WeakAdopted,
                        ),
                        session_incarnation: crate::daemon_protocol::SessionIncarnation(1),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        soft_restart_session(
            &state,
            "oc",
            None,
            dir.path().to_str().unwrap(),
            None,
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["oc"].metadata;
        assert_eq!(metadata.backend_session_id.as_deref(), Some("ses_new"));
        assert_eq!(
            metadata.opencode_binding,
            Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged)
        );
        server.abort();
    }

    #[tokio::test]
    async fn soft_restart_keeps_previous_binding_when_attach_respawn_fails() {
        use axum::Json;
        use axum::Router;
        use axum::routing::post;
        use tokio::net::TcpListener;

        async fn create_session() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "id": "ses_new" }))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/session", post(create_session));
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
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%missing".into()),
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_old".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::WeakAdopted,
                        ),
                        model: Some("old-model".into()),
                        effort: Some("old-effort".into()),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let result = soft_restart_session(
            &state,
            "oc",
            Some("%missing"),
            dir.path().to_str().unwrap(),
            None,
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
            Some("new-model"),
            Some("new-effort"),
        )
        .await;

        assert!(result.is_err());
        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["oc"].metadata;
        assert_eq!(metadata.backend_session_id.as_deref(), Some("ses_old"));
        assert_eq!(
            metadata.opencode_binding,
            Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted)
        );
        assert_eq!(metadata.model.as_deref(), Some("old-model"));
        assert_eq!(metadata.effort.as_deref(), Some("old-effort"));
        server.abort();
    }

    #[tokio::test]
    async fn stale_soft_restart_target_cannot_attach_or_delete_backend() {
        let state = AppState::new_for_test();
        let lease_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "oc".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(10),
        };
        let stale_target = crate::daemon_protocol::ResourceOwner {
            session_id: "oc".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(11),
        };
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%same".into()),
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        session_incarnation: crate::daemon_protocol::SessionIncarnation(12),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
            proto.lifecycle_leases.insert(
                "oc".into(),
                crate::daemon_protocol::LifecycleLease {
                    owner: lease_owner.clone(),
                    phase: crate::daemon_protocol::LifecyclePhase::Restarting,
                    backend: None,
                    backend_session_id: None,
                    backend_session_owner: None,
                    restart_target_owner: Some(stale_target.clone()),
                    restart_previous: None,
                    project_dir: None,
                    project_dir_owner: None,
                    project_dir_cleanup_on_abandon: false,
                    inert_pane: None,
                    inert_pane_owner: None,
                },
            );
        }

        let attach = respawn_opencode_attach_for_session(
            &state,
            &stale_target,
            &stale_target,
            "%same",
            "/tmp",
            "ses_stale",
            1,
            "oc",
        )
        .await;
        let deleted = delete_claimed_restart_backend(
            &state,
            &lease_owner,
            &stale_target,
            1,
            "ses_stale",
            "stale cleanup test",
        )
        .await;

        assert!(attach.is_err());
        assert!(!deleted);
    }

    #[tokio::test]
    async fn soft_restart_does_not_prompt_async_before_attach_succeeds() {
        use axum::Json;
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::net::TcpListener;

        async fn create_session() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "id": "ses_new" }))
        }

        async fn prompt_async(AxumState(calls): AxumState<StdArc<AtomicUsize>>) -> StatusCode {
            calls.fetch_add(1, Ordering::SeqCst);
            StatusCode::NO_CONTENT
        }

        let calls = StdArc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session", post(create_session))
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(calls.clone());
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
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%missing".into()),
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_old".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::WeakAdopted,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let result = soft_restart_session(
            &state,
            "oc",
            Some("%missing"),
            dir.path().to_str().unwrap(),
            Some("queued prompt"),
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
            None,
            None,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn soft_restart_prompt_includes_effective_lifecycle_reminder() {
        use axum::Json;
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        async fn create_session() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "id": "ses_new" }))
        }

        async fn prompt_async(
            AxumState(captured): AxumState<StdArc<Mutex<Option<String>>>>,
            Json(body): Json<serde_json::Value>,
        ) -> StatusCode {
            let text = body["parts"][0]["text"].as_str().unwrap().to_string();
            *captured.lock().await = Some(text);
            StatusCode::NO_CONTENT
        }

        let captured = StdArc::new(Mutex::new(None));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session", post(create_session))
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(captured.clone());
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
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_old".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::WeakAdopted,
                        ),
                        reminder: Some("check the deployment".into()),
                        parent_session: Some("parent-session".into()),
                        idle_policy: Some(crate::daemon_protocol::IdlePolicy::AskParentWhenDone),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let result = soft_restart_session(
            &state,
            "oc",
            None,
            dir.path().to_str().unwrap(),
            Some("queued prompt"),
            None,
            None,
            Some("check the deployment"),
            ParentSessionOverride::SetParent("parent-session".into()),
            Some(crate::daemon_protocol::IdlePolicy::AskParentWhenDone),
            None,
            None,
        )
        .await;

        assert!(result.is_ok());
        let text = captured.lock().await.clone().unwrap();
        assert!(text.starts_with("queued prompt\n\ncheck the deployment\n\n"));
        assert!(text.contains("Lifecycle policy: ask-parent-when-done"));
        assert!(text.contains("Parent session id: parent-session"));
        assert!(text.contains("ouija ask parent-session --stdin --from oc"));
        assert!(!text.contains("ouija clear-reminder"));
        assert!(!text.contains("<clearing_id>"));
        server.abort();
    }

    #[tokio::test]
    async fn soft_restart_applies_lifecycle_overrides_to_prompt_and_metadata() {
        use axum::Json;
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        async fn create_session() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "id": "ses_new" }))
        }

        async fn prompt_async(
            AxumState(captured): AxumState<StdArc<Mutex<Option<String>>>>,
            Json(body): Json<serde_json::Value>,
        ) -> StatusCode {
            let text = body["parts"][0]["text"].as_str().unwrap().to_string();
            *captured.lock().await = Some(text);
            StatusCode::NO_CONTENT
        }

        let captured = StdArc::new(Mutex::new(None));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session", post(create_session))
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(captured.clone());
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
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_old".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::WeakAdopted,
                        ),
                        reminder: Some("old manual reminder".into()),
                        parent_session: Some("old-parent".into()),
                        idle_policy: Some(crate::daemon_protocol::IdlePolicy::AskParentWhenDone),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let result = restart_session(
            &state,
            "oc",
            true,
            None,
            Some("queued prompt"),
            None,
            None,
            Some("opencode"),
            None,
            None,
            Some("new manual reminder"),
            ParentSessionOverride::SetParent("new-parent".into()),
            Some(crate::daemon_protocol::IdlePolicy::CloseWhenDone),
        )
        .await;

        assert!(result.0.starts_with("soft-restarted 'oc'"));
        let text = captured.lock().await.clone().unwrap();
        assert!(text.starts_with("queued prompt\n\nnew manual reminder\n\n"));
        assert!(text.contains("Lifecycle policy: close-when-done"));
        assert!(text.contains("Close command: ouija kill-session oc --keep-worktree"));
        assert!(!text.contains("old-parent"));
        assert!(!text.contains("ask-parent-when-done"));

        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["oc"].metadata;
        assert_eq!(metadata.reminder.as_deref(), Some("new manual reminder"));
        assert_eq!(metadata.parent_session.as_deref(), Some("new-parent"));
        assert_eq!(
            metadata.idle_policy,
            Some(crate::daemon_protocol::IdlePolicy::CloseWhenDone)
        );
        server.abort();
    }

    #[tokio::test]
    async fn soft_fresh_restart_applies_active_context_policy_after_stored_one_shot_acceptance() {
        // Break caught: the OpenCode completion path must apply the requested
        // fresh-context limit and reset accounting only after prompt acceptance.
        use axum::Json;
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        async fn create_session() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "id": "ses_new" }))
        }

        async fn prompt_async(
            AxumState(captured): AxumState<StdArc<Mutex<Option<String>>>>,
            Json(body): Json<serde_json::Value>,
        ) -> StatusCode {
            *captured.lock().await = body["parts"][0]["text"].as_str().map(String::from);
            StatusCode::NO_CONTENT
        }

        let captured = StdArc::new(Mutex::new(None));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session", post(create_session))
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(captured.clone());
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
        let state = AppState::new(config);
        {
            let mut protocol = state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Register {
                id: "soft-fresh".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some(dir.path().display().to_string()),
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_old".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    prompt: Some("stored continuation".into()),
                    fresh_context_after_active_secs: Some(60),
                    active_context_accumulated_secs: 61,
                    active_context_segment_started_at: Some(100),
                    active_context_restart_due: true,
                    ..Default::default()
                },
            });
        }

        let (message, _, outcome) = restart_session_with_prompt_controls(
            &state,
            "soft-fresh",
            true,
            Some(120),
            None,
            None,
            false,
            Some("one-shot continuation"),
            None,
            None,
            Some("opencode"),
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
        )
        .await;

        assert_eq!(outcome, RestartOutcome::Restarted, "{message}");
        assert_eq!(
            captured.lock().await.as_deref(),
            Some("stored continuation\n\none-shot continuation")
        );
        let protocol = state.protocol.read().await;
        let metadata = &protocol.sessions["soft-fresh"].metadata;
        assert_eq!(metadata.fresh_context_after_active_secs, Some(120));
        assert_eq!(metadata.active_context_accumulated_secs, 0);
        assert_eq!(metadata.active_context_segment_started_at, None);
        assert!(!metadata.active_context_restart_due);
        assert!(!metadata.active_context_accounting_provisional);
        assert_eq!(metadata.prompt.as_deref(), Some("stored continuation"));
        server.abort();
    }

    #[tokio::test]
    async fn paneless_soft_restart_preserves_staged_backend_hook_activity_and_due_boundary() {
        // Break caught: the production paneless soft path must route the
        // newly claimed backend's real hooks before prompt acceptance and
        // retain the same target mailbox through completion.
        use axum::Json;
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use tokio::net::TcpListener;

        async fn create_session() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "id": "ses_new" }))
        }

        async fn prompt_async() -> StatusCode {
            StatusCode::NO_CONTENT
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session", post(create_session))
            .route("/session/{session_id}/prompt_async", post(prompt_async));
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
        let state = AppState::new(config);
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "soft-staged".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some(dir.path().display().to_string()),
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_old".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    fresh_context_after_active_secs: Some(1),
                    ..Default::default()
                },
            })
            .await;
        let control = crate::state::RestartTestControl::new(
            crate::state::RestartTestCheckpoint::SoftAfterBackendClaim,
        );
        state.set_restart_test_control(control.clone());
        let restart_state = state.clone();
        let restart = tokio::spawn(async move {
            restart_session_with_prompt_controls(
                &restart_state,
                "soft-staged",
                true,
                Some(1),
                None,
                Some("continue"),
                false,
                None,
                None,
                None,
                Some("opencode"),
                None,
                None,
                None,
                ParentSessionOverride::PreservePrevious,
                None,
            )
            .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            control.reached.notified(),
        )
        .await
        .expect("soft restart did not reach its test checkpoint within 2 seconds");

        let target = state.protocol.read().await.sessions["soft-staged"].owner();
        let _ = crate::hooks::prompt_submit(
            AxumState(state.clone()),
            Json(crate::hooks::PaneBody {
                pane: None,
                backend_session_id: Some("ses_new".into()),
                session_incarnation: Some(target.incarnation),
            }),
        )
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if state.protocol.read().await.sessions["soft-staged"]
                    .metadata
                    .active_context_segment_started_at
                    .is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("staged paneless Active hook must reach the target receiver");
        state
            .protocol
            .write()
            .await
            .sessions
            .get_mut("soft-staged")
            .expect("staged soft owner must remain current")
            .metadata
            .active_context_segment_started_at = Some(chrono::Utc::now().timestamp() - 2);
        let _ = crate::hooks::hook_stop(
            AxumState(state.clone()),
            Json(crate::hooks::PaneBody {
                pane: None,
                backend_session_id: Some("ses_new".into()),
                session_incarnation: Some(target.incarnation),
            }),
        )
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if state.protocol.read().await.sessions["soft-staged"]
                    .metadata
                    .active_context_restart_due
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("staged paneless Stopped hook must record the due boundary");
        assert!(
            state
                .try_set_pending_compact_continuation("soft-staged", "soft staged mailbox".into())
                .await
        );

        control.release.notify_one();
        let (message, _, outcome) =
            tokio::time::timeout(std::time::Duration::from_secs(5), restart)
                .await
                .expect("soft restart did not finish within 5 seconds")
                .expect("soft restart task failed");
        server.abort();
        assert_eq!(outcome, RestartOutcome::Restarted, "{message}");
        let protocol = state.protocol.read().await;
        let metadata = &protocol.sessions["soft-staged"].metadata;
        assert_eq!(protocol.sessions["soft-staged"].owner(), target);
        assert_eq!(metadata.backend_session_id.as_deref(), Some("ses_new"));
        assert!(metadata.active_context_accumulated_secs >= 2);
        assert!(metadata.active_context_restart_due);
        assert!(!metadata.active_context_accounting_provisional);
        drop(protocol);
        assert_eq!(
            state.drain_agent_compact_continuation_owned(&target).await,
            Some("soft staged mailbox".into())
        );
    }

    #[tokio::test]
    async fn soft_restart_prompt_delivery_rejects_known_not_accepted_status() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        use tokio::net::TcpListener;

        async fn prompt_async() -> StatusCode {
            StatusCode::NOT_FOUND
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/session/{session_id}/prompt_async", post(prompt_async));
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
        let state = AppState::new(config);

        let result = deliver_soft_restart_prompt(
            &state,
            port,
            "ses_new",
            dir.path().to_str().unwrap(),
            "queued prompt",
            None,
            None,
        )
        .await;

        assert!(matches!(result, crate::state::DeliveryOutcome::Rejected(_)));
        server.abort();
    }

    #[tokio::test]
    async fn soft_restart_prompt_delivery_accepts_ambiguous_server_error() {
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
        let config = crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        };
        let state = AppState::new(config);

        let result = deliver_soft_restart_prompt(
            &state,
            port,
            "ses_new",
            dir.path().to_str().unwrap(),
            "queued prompt",
            None,
            None,
        )
        .await;

        assert!(matches!(
            result,
            crate::state::DeliveryOutcome::Ambiguous(_)
        ));
        server.abort();
    }

    #[tokio::test]
    async fn soft_restart_prompt_delivery_accepts_transport_error_after_request_body() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let saw_prompt_body = StdArc::new(AtomicBool::new(false));
        let saw_prompt_body2 = saw_prompt_body.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request
                    .windows(b"queued prompt".len())
                    .any(|w| w == b"queued prompt")
                {
                    saw_prompt_body2.store(true, Ordering::SeqCst);
                    break;
                }
            }
            stream.shutdown().await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        };
        let state = AppState::new(config);

        let result = deliver_soft_restart_prompt(
            &state,
            port,
            "ses_new",
            dir.path().to_str().unwrap(),
            "queued prompt",
            None,
            None,
        )
        .await;

        assert!(saw_prompt_body.load(Ordering::SeqCst));
        assert!(matches!(
            result,
            crate::state::DeliveryOutcome::Ambiguous(_)
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn soft_restart_restores_previous_metadata_when_prompt_delivery_fails() {
        use axum::Json;
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        use tokio::net::TcpListener;

        async fn create_session() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "id": "ses_new" }))
        }

        async fn prompt_async() -> StatusCode {
            StatusCode::NOT_FOUND
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session", post(create_session))
            .route("/session/{session_id}/prompt_async", post(prompt_async));
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
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_old".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::WeakAdopted,
                        ),
                        model: Some("old-model".into()),
                        effort: Some("old-effort".into()),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let result = soft_restart_session(
            &state,
            "oc",
            None,
            dir.path().to_str().unwrap(),
            Some("queued prompt"),
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
            Some("new-model"),
            Some("new-effort"),
        )
        .await;

        assert!(result.is_err());
        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["oc"].metadata;
        assert_eq!(metadata.backend_session_id.as_deref(), Some("ses_old"));
        assert_eq!(
            metadata.opencode_binding,
            Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted)
        );
        assert_eq!(metadata.model.as_deref(), Some("old-model"));
        assert_eq!(metadata.effort.as_deref(), Some("old-effort"));
        server.abort();
    }

    #[tokio::test]
    async fn headless_soft_restart_holds_target_lease_through_prompt_async() {
        use axum::Json;
        use axum::Router;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc;
        use tokio::net::TcpListener;

        async fn create_session() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "id": "ses_new" }))
        }

        async fn prompt_async(State(state): State<Arc<AppState>>) -> StatusCode {
            let proto = state.protocol.read().await;
            let session = &proto.sessions["oc"];
            if session.metadata.backend_session_id.is_none()
                && proto.lifecycle_leases.get("oc").is_some_and(|lease| {
                    lease.phase == crate::daemon_protocol::LifecyclePhase::Restarting
                        && lease.restart_target_owner.as_ref() == Some(&session.owner())
                        && lease.backend.as_deref() == Some("opencode")
                        && lease.backend_session_id.as_deref() == Some("ses_new")
                        && lease.backend_session_owner.as_ref() == Some(&session.owner())
                })
            {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        };
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_old".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::WeakAdopted,
                        ),
                        model: Some("old-model".into()),
                        effort: Some("old-effort".into()),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        let app = Router::new()
            .route("/session", post(create_session))
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(Arc::clone(&state));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let result = soft_restart_session(
            &state,
            "oc",
            None,
            dir.path().to_str().unwrap(),
            Some("queued prompt"),
            None,
            None,
            None,
            ParentSessionOverride::PreservePrevious,
            None,
            Some("new-model"),
            Some("new-effort"),
        )
        .await;

        assert!(result.is_ok());
        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["oc"].metadata;
        assert_eq!(metadata.backend_session_id.as_deref(), Some("ses_new"));
        assert_eq!(
            metadata.opencode_binding,
            Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged)
        );
        assert_eq!(metadata.model.as_deref(), Some("new-model"));
        assert_eq!(metadata.effort.as_deref(), Some("new-effort"));
        server.abort();
    }

    #[test]
    fn pane_backed_soft_restart_with_prompt_defers_metadata_commit() {
        assert!(!should_commit_soft_restart_metadata_before_prompt(
            Some("%1"),
            Some("queued prompt")
        ));
        assert!(!should_commit_soft_restart_metadata_before_prompt(
            None,
            Some("queued prompt")
        ));
        assert!(should_commit_soft_restart_metadata_before_prompt(
            Some("%1"),
            None
        ));
    }

    #[test]
    fn pane_backed_soft_restart_prompt_failure_reattaches_previous_backend_session() {
        let metadata = crate::daemon_protocol::SessionMeta {
            backend: Some("opencode".into()),
            backend_session_id: Some("ses_old".into()),
            opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
            ..Default::default()
        };

        assert_eq!(
            previous_backend_session_for_prompt_failure_rollback(Some("%1"), &metadata),
            Some("ses_old")
        );
        assert_eq!(
            previous_backend_session_for_prompt_failure_rollback(None, &metadata),
            None
        );
    }

    #[tokio::test]
    async fn soft_restart_metadata_commit_rejects_stale_generation() {
        let state = AppState::new_for_test();
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_current".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        restart_generation: 1,
                        session_incarnation: crate::daemon_protocol::SessionIncarnation(1),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let owner = SoftRestartOwnerSnapshot {
            session_id: "oc".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(1),
        };
        let result = apply_soft_restart_metadata(
            &state,
            &owner,
            "ses_stale",
            0,
            SoftRestartMetadataUpdate::default(),
        )
        .await;

        assert!(result.is_err());
        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["oc"].metadata;
        assert_eq!(metadata.backend_session_id.as_deref(), Some("ses_current"));
        assert_eq!(metadata.restart_generation, 1);
    }

    #[tokio::test]
    async fn soft_restart_metadata_commit_rejects_recreated_session_with_same_generation() {
        let state = AppState::new_for_test();
        let owner = SoftRestartOwnerSnapshot {
            session_id: "oc".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(1),
        };
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_recreated".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        restart_generation: 0,
                        session_incarnation: crate::daemon_protocol::SessionIncarnation(2),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let result = apply_soft_restart_metadata(
            &state,
            &owner,
            "ses_stale",
            0,
            SoftRestartMetadataUpdate::default(),
        )
        .await;

        assert!(result.is_err());
        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["oc"].metadata;
        assert_eq!(
            metadata.backend_session_id.as_deref(),
            Some("ses_recreated")
        );
        assert_eq!(metadata.restart_generation, 0);
    }

    #[tokio::test]
    async fn soft_restart_metadata_commit_no_parent_clears_previous_parent() {
        let state = AppState::new_for_test();
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend_session_id: Some("ses_old".into()),
                        parent_session: Some("old-parent".into()),
                        restart_generation: 0,
                        session_incarnation: crate::daemon_protocol::SessionIncarnation(1),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let owner = SoftRestartOwnerSnapshot {
            session_id: "oc".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(1),
        };
        apply_soft_restart_metadata(
            &state,
            &owner,
            "ses_new",
            0,
            SoftRestartMetadataUpdate {
                parent_session: ParentSessionOverride::NoParent,
                ..Default::default()
            },
        )
        .await
        .expect("metadata commit should succeed");

        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["oc"].metadata;
        assert_eq!(metadata.backend_session_id.as_deref(), Some("ses_new"));
        assert_eq!(metadata.parent_session, None);
        assert_eq!(metadata.restart_generation, 1);
    }

    #[tokio::test]
    async fn soft_restart_metadata_commit_sets_opencode_backend() {
        let state = AppState::new_for_test();
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        session_incarnation: crate::daemon_protocol::SessionIncarnation(1),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        let owner = SoftRestartOwnerSnapshot {
            session_id: "oc".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(1),
        };

        apply_soft_restart_metadata(
            &state,
            &owner,
            "ses_new",
            0,
            SoftRestartMetadataUpdate::default(),
        )
        .await
        .expect("metadata commit should succeed");

        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["oc"].metadata;
        assert_eq!(metadata.backend.as_deref(), Some("opencode"));
        assert_eq!(metadata.backend_session_id.as_deref(), Some("ses_new"));
        assert_eq!(
            metadata.opencode_binding,
            Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged)
        );
    }

    #[tokio::test]
    async fn failed_soft_restart_commit_rolls_back_to_winning_backend_session() {
        let state = AppState::new_for_test();
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%1".into()),
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_winner".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        restart_generation: 2,
                        session_incarnation: crate::daemon_protocol::SessionIncarnation(1),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        let previous_metadata = crate::daemon_protocol::SessionMeta {
            backend: Some("opencode".into()),
            ..Default::default()
        };

        let target =
            failed_soft_restart_commit_rollback_target(&state, "oc", &previous_metadata).await;

        assert_eq!(target.as_deref(), Some("ses_winner"));
    }

    #[tokio::test]
    async fn soft_restart_metadata_restore_resets_restart_generation() {
        let state = AppState::new_for_test();
        let previous_metadata = crate::daemon_protocol::SessionMeta {
            backend: Some("opencode".into()),
            backend_session_id: Some("ses_old".into()),
            opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted),
            model: Some("old-model".into()),
            effort: Some("old-effort".into()),
            restart_generation: 7,
            session_incarnation: crate::daemon_protocol::SessionIncarnation(1),
            ..Default::default()
        };
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%1".into()),
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: previous_metadata.clone(),
                    registered_at: 0,
                },
            );
        }

        let owner = SoftRestartOwnerSnapshot {
            session_id: "oc".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(1),
        };
        apply_soft_restart_metadata(
            &state,
            &owner,
            "ses_new",
            7,
            SoftRestartMetadataUpdate {
                model: Some("new-model"),
                ..Default::default()
            },
        )
        .await
        .expect("metadata commit should succeed before simulated prompt failure");
        restore_soft_restart_metadata(&state, "oc", "ses_new", &previous_metadata).await;

        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["oc"].metadata;
        assert_eq!(metadata.backend_session_id.as_deref(), Some("ses_old"));
        assert_eq!(
            metadata.opencode_binding,
            Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted)
        );
        assert_eq!(metadata.model.as_deref(), Some("old-model"));
        assert_eq!(metadata.effort.as_deref(), Some("old-effort"));
        assert_eq!(metadata.restart_generation, 7);
    }

    #[tokio::test]
    async fn soft_restart_metadata_restore_resets_backend() {
        let state = AppState::new_for_test();
        let previous_metadata = crate::daemon_protocol::SessionMeta {
            backend: None,
            backend_session_id: None,
            opencode_binding: None,
            restart_generation: 3,
            session_incarnation: crate::daemon_protocol::SessionIncarnation(1),
            ..Default::default()
        };
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%1".into()),
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: previous_metadata.clone(),
                    registered_at: 0,
                },
            );
        }
        let owner = SoftRestartOwnerSnapshot {
            session_id: "oc".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(1),
        };
        apply_soft_restart_metadata(
            &state,
            &owner,
            "ses_new",
            3,
            SoftRestartMetadataUpdate::default(),
        )
        .await
        .expect("metadata commit should succeed before simulated prompt failure");

        restore_soft_restart_metadata(&state, "oc", "ses_new", &previous_metadata).await;

        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["oc"].metadata;
        assert_eq!(metadata.backend, None);
        assert_eq!(metadata.backend_session_id, None);
        assert_eq!(metadata.opencode_binding, None);
        assert_eq!(metadata.restart_generation, 3);
    }

    #[test]
    fn restart_previous_session_reuse_requires_verified_attach() {
        let result = previous_backend_session_after_attach("ses_prev".to_string(), false, "%1");
        assert!(result.is_err());
    }

    #[test]
    fn restart_prompt_injection_requires_backend_session() {
        assert!(!should_schedule_restart_prompt_injection(
            true,
            None,
            Some(&crate::daemon_protocol::OpenCodeBinding::StrongManaged),
        ));
    }

    #[test]
    fn restart_prompt_injection_requires_strong_opencode_binding() {
        assert!(!should_schedule_restart_prompt_injection(
            true,
            Some("previous-session"),
            Some(&crate::daemon_protocol::OpenCodeBinding::WeakAdopted),
        ));
    }

    #[test]
    fn opencode_prompt_body_with_model_two_segments() {
        let body = opencode_prompt_body("hi", Some("openrouter/gpt-5"), None);
        assert_eq!(
            body["model"],
            serde_json::json!({
                "providerID": "openrouter",
                "modelID": "gpt-5",
            })
        );
        assert!(body.get("variant").is_none());
    }

    #[test]
    fn opencode_prompt_body_with_model_three_segments_splits_on_first_slash() {
        // opencode's parser splits on the FIRST `/` only.
        // `openrouter/openai/gpt-5.4` -> provider=openrouter, model=openai/gpt-5.4
        let body = opencode_prompt_body("hi", Some("openrouter/openai/gpt-5.4"), None);
        assert_eq!(
            body["model"],
            serde_json::json!({
                "providerID": "openrouter",
                "modelID": "openai/gpt-5.4",
            })
        );
    }

    #[test]
    fn opencode_prompt_body_with_effort_only() {
        let body = opencode_prompt_body("hi", None, Some("xhigh"));
        assert!(body.get("model").is_none());
        assert_eq!(body["variant"], serde_json::Value::String("xhigh".into()));
    }

    #[test]
    fn opencode_prompt_body_with_model_and_effort() {
        let body = opencode_prompt_body(
            "do the thing",
            Some("openrouter/google/gemini-3.1-pro-preview"),
            Some("xhigh"),
        );
        assert_eq!(
            body["model"],
            serde_json::json!({
                "providerID": "openrouter",
                "modelID": "google/gemini-3.1-pro-preview",
            })
        );
        assert_eq!(body["variant"], serde_json::Value::String("xhigh".into()));
        assert_eq!(
            body["parts"],
            serde_json::json!([{"type": "text", "text": "do the thing"}])
        );
    }

    #[test]
    fn opencode_prompt_body_with_model_no_slash_omits_model() {
        // No `/` is ambiguous (no providerID). Omit the model field and let
        // opencode fall back to the agent / session default.
        let body = opencode_prompt_body("hi", Some("sonnet"), Some("max"));
        assert!(
            body.get("model").is_none(),
            "no-slash model must be omitted, got: {body}"
        );
        assert_eq!(body["variant"], serde_json::Value::String("max".into()));
    }

    #[test]
    fn opencode_prompt_body_omits_malformed_slash_values() {
        // `/`, `openrouter/`, `/gpt-5` all have an empty segment and must be
        // treated as ambiguous.
        for bad in ["/", "openrouter/", "/gpt-5", " / ", "   "] {
            let body = opencode_prompt_body("hi", Some(bad), None);
            assert!(
                body.get("model").is_none(),
                "malformed model {bad:?} must be omitted, got: {body}"
            );
        }
    }

    #[test]
    fn opencode_prompt_body_omits_empty_variant() {
        // An empty effort string must not be sent as variant = "".
        let body = opencode_prompt_body("hi", None, Some(""));
        assert!(
            body.get("variant").is_none(),
            "empty effort must be omitted, got: {body}"
        );
    }

    #[test]
    fn opencode_prompt_body_trims_padded_segments() {
        // `"openrouter / gpt-5"` splits into `"openrouter "` and `" gpt-5"`.
        // Before the fix, both trimmed-non-empty-guarded segments were
        // inserted UN-trimmed, yielding providerID=" openrouter " — which
        // opencode's provider lookup would not match.
        let body = opencode_prompt_body("hi", Some("openrouter / gpt-5"), None);
        assert_eq!(
            body["model"],
            serde_json::json!({
                "providerID": "openrouter",
                "modelID": "gpt-5",
            }),
            "segments must be trimmed before insertion, got: {body}"
        );
    }

    #[test]
    fn opencode_prompt_body_rejects_whitespace_only_segment() {
        // `"openrouter / "` trims the model_id segment to empty and must be
        // omitted, not inserted as providerID="openrouter", modelID="".
        let body = opencode_prompt_body("hi", Some("openrouter / "), None);
        assert!(
            body.get("model").is_none(),
            "whitespace-only modelID must be rejected, got: {body}"
        );
        let body = opencode_prompt_body("hi", Some(" / gpt-5"), None);
        assert!(
            body.get("model").is_none(),
            "whitespace-only providerID must be rejected, got: {body}"
        );
    }

    #[test]
    fn opencode_prompt_body_trims_padded_effort() {
        // Whitespace padding on effort must not flow through as variant.
        let body = opencode_prompt_body("hi", None, Some("  max  "));
        assert_eq!(
            body["variant"],
            serde_json::Value::String("max".into()),
            "effort must be trimmed before insertion, got: {body}"
        );
    }

    // --- create_ouija_worktree: branch-wipe guard tests ---
    //
    // Regression coverage for hub#528 (2026-04-21): `create_ouija_worktree`
    // used to unconditionally `git checkout -B <branch> <base>` on any
    // existing worktree dir when the caller passed `base_branch`, silently
    // discarding every commit the branch was ahead of base.

    /// Build a throwaway git repo with one commit on its default branch,
    /// and add a worktree on a new branch named `branch` (starting from
    /// base). Returns `(repo_dir_keep_alive, worktree_dir, base_branch)`.
    ///
    /// Subsequent commits inside the worktree will put `branch` ahead of
    /// `base` so the guard can be exercised.
    fn setup_repo_with_worktree(
        home: &std::path::Path,
        name: &str,
        branch: &str,
    ) -> (tempfile::TempDir, String, String) {
        use std::process::Command;
        let repo = tempfile::tempdir().expect("tempdir for repo");
        let repo_dir = repo.path().to_str().unwrap().to_string();

        // Isolate from any user git config / hooks so tests are
        // reproducible regardless of host environment.
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .env("HOME", home)
                // Disable GPG signing regardless of host config.
                .env("GIT_CONFIG_COUNT", "2")
                .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
                .env("GIT_CONFIG_VALUE_0", "false")
                .env("GIT_CONFIG_KEY_1", "tag.gpgsign")
                .env("GIT_CONFIG_VALUE_1", "false")
                // Skip global/system config so host-level hooks/templates
                // cannot fail the commit.
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git ran");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out
        };

        run(&["-C", &repo_dir, "init", "-q", "--initial-branch=main"]);
        std::fs::write(format!("{repo_dir}/README"), "r").unwrap();
        run(&["-C", &repo_dir, "add", "README"]);
        run(&["-C", &repo_dir, "commit", "-q", "-m", "init"]);

        // Create the worktree dir at the new-location path the function
        // uses, so the "existing worktree" branch of the code fires.
        let repo_slug = std::path::Path::new(&repo_dir)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        let wt_parent = home.join(".ouija/worktrees").join(repo_slug);
        std::fs::create_dir_all(&wt_parent).unwrap();
        let wt_dir = wt_parent.join(name).to_str().unwrap().to_string();

        run(&[
            "-C", &repo_dir, "worktree", "add", "-b", branch, &wt_dir, "main",
        ]);

        (repo, wt_dir, "main".to_string())
    }

    fn commit_in(wt_dir: &str, filename: &str, msg: &str) -> String {
        use std::process::Command;
        std::fs::write(format!("{wt_dir}/{filename}"), "data").unwrap();
        // Match `setup_repo_with_worktree`: clear env hooks that might
        // interact with the host's git config (GPG signing, commit
        // template, hook path) to prevent test flakes.
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                // Disable GPG signing regardless of host config.
                .env("GIT_CONFIG_COUNT", "2")
                .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
                .env("GIT_CONFIG_VALUE_0", "false")
                .env("GIT_CONFIG_KEY_1", "tag.gpgsign")
                .env("GIT_CONFIG_VALUE_1", "false")
                // Skip global/system config so host-level hooks/templates
                // cannot fail the commit.
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git ran")
        };
        let o = run(&["-C", wt_dir, "add", filename]);
        assert!(
            o.status.success(),
            "git add {filename} in {wt_dir}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let o = run(&["-C", wt_dir, "commit", "-q", "-m", msg]);
        assert!(
            o.status.success(),
            "git commit in {wt_dir}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let sha = run(&["-C", wt_dir, "rev-parse", "HEAD"]);
        String::from_utf8_lossy(&sha.stdout).trim().to_string()
    }

    fn branch_tip(wt_dir: &str, branch: &str) -> String {
        let out = std::process::Command::new("git")
            .args(["-C", wt_dir, "rev-parse", branch])
            .output()
            .expect("git ran");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Helper: set up repo+worktree and return (repo_keepalive, repo_dir,
    /// wt_dir, base_branch). `home` will be passed explicitly into
    /// `create_ouija_worktree` (no HOME env mutation — tests run in
    /// parallel and must not share global state).
    fn repo_and_worktree(
        home: &std::path::Path,
        name: &str,
        branch: &str,
    ) -> (tempfile::TempDir, String, String, String) {
        let (repo, wt_dir, base) = setup_repo_with_worktree(home, name, branch);
        let repo_dir = repo.path().to_str().unwrap().to_string();
        (repo, repo_dir, wt_dir, base)
    }

    #[test]
    fn existing_worktree_with_ahead_commits_not_reset_by_default() {
        let home = tempfile::tempdir().unwrap();
        let (_repo, repo_dir, wt_dir, base) = repo_and_worktree(home.path(), "s1", "feat-x");

        // Put the branch 2 commits ahead of base.
        commit_in(&wt_dir, "a", "a");
        let tip_before = commit_in(&wt_dir, "b", "b");

        let out = create_ouija_worktree(
            &repo_dir,
            "s1",
            Some("feat-x"),
            Some(&base),
            /* force_reset = */ false,
            home.path(),
        )
        .unwrap();
        assert_eq!(out, wt_dir);

        let tip_after = branch_tip(&wt_dir, "feat-x");
        assert_eq!(
            tip_before, tip_after,
            "branch must not be silently reset when force_reset=false \
             and branch is ahead of base"
        );
    }

    #[test]
    fn existing_worktree_with_ahead_commits_reset_when_forced() {
        let home = tempfile::tempdir().unwrap();
        let (_repo, repo_dir, wt_dir, base) = repo_and_worktree(home.path(), "s2", "feat-y");
        let base_tip = branch_tip(&wt_dir, &base);
        commit_in(&wt_dir, "a", "a");
        commit_in(&wt_dir, "b", "b");

        let _ = create_ouija_worktree(
            &repo_dir,
            "s2",
            Some("feat-y"),
            Some(&base),
            /* force_reset = */ true,
            home.path(),
        )
        .unwrap();

        let tip_after = branch_tip(&wt_dir, "feat-y");
        assert_eq!(
            base_tip, tip_after,
            "force_reset=true must reset branch to base (current behavior)"
        );
    }

    #[test]
    fn existing_worktree_with_no_ahead_commits_is_safe_noop() {
        let home = tempfile::tempdir().unwrap();
        let (_repo, repo_dir, wt_dir, base) = repo_and_worktree(home.path(), "s3", "feat-z");
        // No commits beyond base. Branch is at base already.
        let base_tip = branch_tip(&wt_dir, &base);

        let _ = create_ouija_worktree(
            &repo_dir,
            "s3",
            Some("feat-z"),
            Some(&base),
            /* force_reset = */ false,
            home.path(),
        )
        .unwrap();

        let tip_after = branch_tip(&wt_dir, "feat-z");
        assert_eq!(
            base_tip, tip_after,
            "not-ahead branch must remain at base (no-op, not an error)"
        );
    }

    #[test]
    fn missing_base_branch_ref_does_not_silently_reset() {
        let home = tempfile::tempdir().unwrap();
        let (_repo, repo_dir, wt_dir, _base) = repo_and_worktree(home.path(), "s4", "feat-q");
        let tip_before = commit_in(&wt_dir, "a", "a");

        let _ = create_ouija_worktree(
            &repo_dir,
            "s4",
            Some("feat-q"),
            Some("does-not-exist-branch"),
            /* force_reset = */ false,
            home.path(),
        )
        .unwrap();

        let tip_after = branch_tip(&wt_dir, "feat-q");
        assert_eq!(
            tip_before, tip_after,
            "missing base ref must fail safe: skip the reset, preserve work"
        );
    }

    /// When `force_reset=true`, the caller has explicitly asked for the
    /// destructive reset. If the ahead-of-base check cannot be computed
    /// (e.g. the branch ref does not exist yet in this worktree), ouija
    /// must still honor the request rather than silently dropping it.
    /// Construct a case where the ref check fails but the checkout would
    /// succeed: the worktree dir exists but the requested branch does
    /// not yet exist inside it — `git rev-list --count base..newbranch`
    /// returns non-zero (unknown revision), but `git checkout -B
    /// newbranch base` succeeds and creates the branch at base.
    ///
    /// Old behavior (reviewed): return Ok without attempting the reset,
    /// dropping the caller's explicit intent. New behavior: honor
    /// force_reset=true and attempt the reset anyway.
    #[test]
    fn force_reset_true_honored_even_when_rev_list_fails() {
        let home = tempfile::tempdir().unwrap();
        let (_repo, repo_dir, wt_dir, base) = repo_and_worktree(home.path(), "s5", "feat-initial");

        // Delete the initial branch ref so rev-list cannot compute
        // `base..feat-initial`. The worktree dir still exists on disk,
        // which is the scenario the function is guarding.
        //
        // `git branch -D` refuses to delete the branch currently checked
        // out in a worktree, so first detach HEAD.
        let run_in_wt = |args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(["-C", &wt_dir])
                .args(args)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git -C {wt_dir} {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        run_in_wt(&["checkout", "--detach", "-q"]);
        run_in_wt(&["branch", "-D", "feat-initial"]);

        // Sanity: branch really is gone, so rev-list will fail.
        assert!(
            git_rev_count(&wt_dir, &base, "feat-initial").is_none(),
            "test setup: rev-list must fail when branch is absent"
        );

        let _ = create_ouija_worktree(
            &repo_dir,
            "s5",
            Some("feat-initial"),
            Some(&base),
            /* force_reset = */ true,
            home.path(),
        )
        .unwrap();

        // If force_reset is honored, checkout -B creates feat-initial
        // at base. If it is silently dropped, feat-initial remains absent.
        let tip = git_rev_parse(&wt_dir, "feat-initial");
        assert_eq!(
            tip,
            Some(branch_tip(&wt_dir, &base)),
            "force_reset=true must be honored when rev-list fails: \
             checkout -B should create feat-initial at base"
        );
    }

    /// When `force_reset=true` and the subsequent `git checkout -B` also
    /// fails, the failure must be surfaced to the caller as an `Err`.
    /// Returning `Ok(wt_dir)` conflates a successful destructive reset
    /// with a failed one — the caller (hub) has no way to know its
    /// explicit opt-in was dropped, and start_session will proceed on
    /// whatever HEAD the worktree happens to have.
    ///
    /// Construct the worst case: delete both the branch ref and the base
    /// ref in the worktree. rev-list fails (→ None arm with
    /// force_reset=true), then `checkout -B branch base` also fails with
    /// "base is not a commit". Previously this returned Ok silently; now
    /// it must return Err.
    #[test]
    fn force_reset_true_propagates_when_reset_fails() {
        let home = tempfile::tempdir().unwrap();
        let (_repo, repo_dir, wt_dir, base) = repo_and_worktree(home.path(), "s6", "feat-lost");

        // Detach HEAD in both the main worktree (to free `main`) and the
        // added worktree (to free `feat-lost`), then delete both refs so
        // rev-list AND checkout -B will fail.
        let run_in = |dir: &str, args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(["-C", dir])
                .args(args)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git -C {dir} {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        run_in(&repo_dir, &["checkout", "--detach", "-q"]);
        run_in(&repo_dir, &["branch", "-D", &base]);
        run_in(&wt_dir, &["checkout", "--detach", "-q"]);
        run_in(&wt_dir, &["branch", "-D", "feat-lost"]);

        // Sanity: both refs are gone, so checkout -B feat-lost main will fail.
        assert!(
            git_rev_count(&wt_dir, &base, "feat-lost").is_none(),
            "test setup: rev-list must fail with both refs missing"
        );

        let result = create_ouija_worktree(
            &repo_dir,
            "s6",
            Some("feat-lost"),
            Some(&base),
            /* force_reset = */ true,
            home.path(),
        );

        assert!(
            result.is_err(),
            "create_ouija_worktree must return Err when force_reset=true \
             is asserted but the reset fails; got Ok({:?})",
            result.ok()
        );
    }

    /// Same invariant as `force_reset_true_propagates_when_reset_fails`,
    /// but on the Some(0) / zero-ahead arm. A caller that opts in with
    /// `force_reset=true` on a branch that is content-equivalent to base
    /// still wants to know if the alignment reset actually ran — the
    /// arm was previously `let _ = run_reset(...)`, swallowing the
    /// failure and returning Ok(wt_dir) indistinguishable from success.
    ///
    /// Construct a zero-ahead scenario where `git checkout -B` fails:
    /// create two worktrees sharing the same repo, both on branch
    /// `shared`. Pass `branch=shared, base=shared` to
    /// `create_ouija_worktree` so rev-list returns 0 and the Some(0)
    /// arm fires; the subsequent `git checkout -B shared shared` in
    /// the second worktree fails because `shared` is held elsewhere.
    #[test]
    fn force_reset_true_propagates_when_zero_ahead_reset_fails() {
        let home = tempfile::tempdir().unwrap();
        let (_repo, repo_dir, wt_dir, base) =
            repo_and_worktree(home.path(), "s7", "held-elsewhere");

        // Make a second worktree that claims the branch we are going to
        // try to checkout inside wt_dir. After this, `git checkout -B
        // held-elsewhere <base>` inside wt_dir will fail because
        // `held-elsewhere` is already used by the other worktree.
        //
        // But first we need to not be on that branch in wt_dir
        // ourselves — detach HEAD so the branch is free to be held by
        // the second worktree.
        let run_in = |dir: &str, args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(["-C", dir])
                .args(args)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git -C {dir} {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        run_in(&wt_dir, &["checkout", "--detach", "-q"]);
        // Put the second worktree on held-elsewhere next to the first.
        let other_wt = home.path().join("other-wt");
        run_in(
            &repo_dir,
            &[
                "worktree",
                "add",
                "-q",
                other_wt.to_str().unwrap(),
                "held-elsewhere",
            ],
        );

        // Sanity: rev-list succeeds (branch and base both resolve), and
        // the branch is 0 ahead of itself.
        assert_eq!(
            git_rev_count(&wt_dir, "held-elsewhere", "held-elsewhere"),
            Some(0),
            "test setup: rev-list must report 0 ahead for Some(0) arm to fire"
        );

        // Call with branch=held-elsewhere, base=held-elsewhere so the
        // Some(0) arm fires inside create_ouija_worktree. The checkout
        // -B should then fail because held-elsewhere is claimed by
        // other_wt.
        let _ = base; // base_branch is "main" in the helper; use held-elsewhere explicitly
        let result = create_ouija_worktree(
            &repo_dir,
            "s7",
            Some("held-elsewhere"),
            Some("held-elsewhere"),
            /* force_reset = */ true,
            home.path(),
        );

        assert!(
            result.is_err(),
            "create_ouija_worktree must return Err when force_reset=true \
             is asserted on a zero-ahead branch but the alignment reset \
             fails; got Ok({:?})",
            result.ok()
        );
    }

    /// When `force_reset=false` and the branch is already 0 ahead of
    /// base, a transient alignment failure must NOT block session start.
    /// The caller did not opt in to destructive behavior; run_reset is
    /// a best-effort HEAD/working-tree alignment. A failure here should
    /// be warn-logged (see follow-on log) but returned as Ok(wt_dir).
    #[test]
    fn zero_ahead_without_force_reset_tolerates_alignment_failure() {
        let home = tempfile::tempdir().unwrap();
        let (_repo, repo_dir, wt_dir, _base) =
            repo_and_worktree(home.path(), "s8", "held-elsewhere2");

        let run_in = |dir: &str, args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(["-C", dir])
                .args(args)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git -C {dir} {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        run_in(&wt_dir, &["checkout", "--detach", "-q"]);
        let other_wt = home.path().join("other-wt-2");
        run_in(
            &repo_dir,
            &[
                "worktree",
                "add",
                "-q",
                other_wt.to_str().unwrap(),
                "held-elsewhere2",
            ],
        );

        // Force the Some(0) arm to fire with force_reset=false. Even
        // though the alignment checkout will fail (branch held
        // elsewhere), the function must still return Ok(wt_dir).
        let result = create_ouija_worktree(
            &repo_dir,
            "s8",
            Some("held-elsewhere2"),
            Some("held-elsewhere2"),
            /* force_reset = */ false,
            home.path(),
        );
        assert!(
            result.is_ok(),
            "force_reset=false + alignment failure must return Ok(wt_dir); got Err({:?})",
            result.err()
        );
    }

    #[test]
    fn missing_worktree_with_ahead_branch_not_reset_by_default() {
        let home = tempfile::tempdir().unwrap();
        let (_repo, repo_dir, wt_dir, base) = repo_and_worktree(home.path(), "s9", "feat-missing");
        commit_in(&wt_dir, "a", "a");
        let tip_before = commit_in(&wt_dir, "b", "b");

        let out = std::process::Command::new("git")
            .args(["-C", &repo_dir, "worktree", "remove", &wt_dir])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git worktree remove {wt_dir}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !std::path::Path::new(&wt_dir).exists(),
            "test setup: worktree directory must be absent"
        );

        let out = create_ouija_worktree(
            &repo_dir,
            "s9",
            Some("feat-missing"),
            Some(&base),
            /* force_reset = */ false,
            home.path(),
        )
        .expect("missing worktree should be recreated without resetting ahead branch");
        assert_eq!(out, wt_dir);

        assert_eq!(
            tip_before,
            branch_tip(&wt_dir, "feat-missing"),
            "missing worktree creation must not reset an ahead branch when force_reset=false"
        );
        assert_eq!(
            tip_before,
            branch_tip(&wt_dir, "HEAD"),
            "recreated worktree must check out the preserved branch tip"
        );
    }

    #[test]
    fn missing_worktree_with_ahead_branch_resets_when_forced() {
        let home = tempfile::tempdir().unwrap();
        let (_repo, repo_dir, wt_dir, base) =
            repo_and_worktree(home.path(), "s10", "feat-missing-forced");
        let base_tip = branch_tip(&wt_dir, &base);
        commit_in(&wt_dir, "a", "a");
        commit_in(&wt_dir, "b", "b");

        let out = std::process::Command::new("git")
            .args(["-C", &repo_dir, "worktree", "remove", &wt_dir])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git worktree remove {wt_dir}: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let out = create_ouija_worktree(
            &repo_dir,
            "s10",
            Some("feat-missing-forced"),
            Some(&base),
            /* force_reset = */ true,
            home.path(),
        )
        .expect("force_reset=true should recreate worktree by resetting branch to base");
        assert_eq!(out, wt_dir);

        assert_eq!(
            base_tip,
            branch_tip(&wt_dir, "feat-missing-forced"),
            "force_reset=true must keep the explicit destructive reset behavior"
        );
        assert_eq!(
            base_tip,
            branch_tip(&wt_dir, "HEAD"),
            "recreated worktree HEAD must match the forced reset target"
        );
    }

    #[test]
    fn missing_worktree_force_reset_propagates_unresolvable_base_failure() {
        let home = tempfile::tempdir().unwrap();
        let (_repo, repo_dir, wt_dir, base) =
            repo_and_worktree(home.path(), "s11", "feat-missing-lost-base");
        commit_in(&wt_dir, "a", "a");
        let tip_before = commit_in(&wt_dir, "b", "b");

        let run_in = |dir: &str, args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(["-C", dir])
                .args(args)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git -C {dir} {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        run_in(&repo_dir, &["worktree", "remove", &wt_dir]);
        run_in(&repo_dir, &["checkout", "--detach", "-q"]);
        run_in(&repo_dir, &["branch", "-D", &base]);

        assert!(
            !std::path::Path::new(&wt_dir).exists(),
            "test setup: worktree directory must be absent"
        );
        assert!(
            git_rev_parse(&repo_dir, &base).is_none(),
            "test setup: base ref must be absent"
        );

        let result = create_ouija_worktree(
            &repo_dir,
            "s11",
            Some("feat-missing-lost-base"),
            Some(&base),
            /* force_reset = */ true,
            home.path(),
        );

        assert!(
            result.is_err(),
            "force_reset=true must return Err when missing-worktree reset fails; got Ok({:?})",
            result.ok()
        );
        assert_eq!(
            tip_before,
            branch_tip(&repo_dir, "feat-missing-lost-base"),
            "failed forced reset must not move the existing branch tip"
        );
    }

    /// Regression: the guard must not break the happy path that creates a
    /// brand-new worktree. A fresh repo + no pre-existing worktree dir
    /// should produce a working checkout on the requested branch.
    #[test]
    fn fresh_worktree_creation_still_works() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let run = |args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .env("HOME", home.path())
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        run(&["-C", &repo_dir, "init", "-q", "--initial-branch=main"]);
        std::fs::write(format!("{repo_dir}/README"), "r").unwrap();
        run(&["-C", &repo_dir, "add", "README"]);
        run(&["-C", &repo_dir, "commit", "-q", "-m", "init"]);

        let out = create_ouija_worktree(
            &repo_dir,
            "fresh",
            Some("feat-new"),
            Some("main"),
            /* force_reset = */ false,
            home.path(),
        )
        .expect("fresh worktree creates cleanly");

        // Directory should exist and contain the file from main.
        assert!(std::path::Path::new(&out).exists());
        assert!(std::path::Path::new(&format!("{out}/README")).exists());
        assert_eq!(branch_tip(&out, "feat-new"), branch_tip(&out, "main"));
    }

    // --- Legacy-path dropped-intent predicate (hub#528 review) ---
    //
    // The legacy short-circuit returns `Ok(legacy_dir)` without running
    // any reset logic or honoring force_reset. That is correct for
    // running-session compatibility, but silently drops an explicit
    // `force_reset=true` opt-in. `legacy_drops_destructive_intent` is
    // the single-source predicate the caller consults before emitting a
    // WARN log; tests cover it so a future refactor cannot accidentally
    // silence the drop.

    #[test]
    fn legacy_drops_destructive_intent_fires_for_force_reset_true() {
        assert!(
            legacy_drops_destructive_intent(Some("main"), true).is_some(),
            "force_reset=true + base_branch must produce a warn"
        );
    }

    #[test]
    fn legacy_drops_destructive_intent_silent_when_no_force_reset() {
        // base_branch alone (without force_reset) is a safe default —
        // the guard on the non-legacy path would also skip when the
        // branch is ahead. Don't warn for that.
        assert!(
            legacy_drops_destructive_intent(Some("main"), false).is_none(),
            "force_reset=false on legacy path is not a dropped intent"
        );
    }

    #[test]
    fn legacy_drops_destructive_intent_silent_when_no_base_branch() {
        // Without base_branch, even the new-path code takes no reset
        // action. Nothing would have been dropped regardless of path.
        assert!(
            legacy_drops_destructive_intent(None, true).is_none(),
            "no base_branch means no reset target; nothing dropped"
        );
    }

    /// Legacy-location worktrees predate the guard. When the caller did
    /// NOT opt in with force_reset=true, the function must return the
    /// legacy dir as-is without running any reset logic — even when
    /// base_branch is supplied. Protects running sessions still under
    /// `<repo>/.ouija/worktrees/<name>`.
    #[test]
    fn legacy_location_short_circuits_when_force_reset_is_false() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let legacy = format!("{repo_dir}/.ouija/worktrees/legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        // Drop a sentinel file so we can confirm nothing inside was touched.
        std::fs::write(format!("{legacy}/SENTINEL"), "untouched").unwrap();

        let out = create_ouija_worktree(
            &repo_dir,
            "legacy",
            Some("any-branch"),
            Some("any-base"),
            /* force_reset = */ false,
            home.path(),
        )
        .expect("legacy path with force_reset=false returns Ok");

        assert_eq!(
            out, legacy,
            "legacy path must be returned verbatim without running any git command"
        );
        assert_eq!(
            std::fs::read_to_string(format!("{legacy}/SENTINEL")).unwrap(),
            "untouched",
            "legacy worktree contents must not be altered"
        );
    }

    /// Mirror of the new-path force_reset=true invariant: when the
    /// caller explicitly opts in with `force_reset=true + base_branch`
    /// but lands on a legacy worktree that cannot honor the reset, the
    /// function must return Err. Otherwise Ok(legacy_dir) is
    /// indistinguishable from a honored reset — the same dropped-intent
    /// shape the non-legacy arms (Some(0)/Some(n)/None) now propagate
    /// via `?`. Blast radius is narrow today, but the first redraft
    /// call site that lands on a legacy worktree would otherwise
    /// silently proceed on unexpected branch state.
    #[test]
    fn legacy_location_returns_err_when_force_reset_true() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_dir = repo.path().to_str().unwrap().to_string();
        let legacy = format!("{repo_dir}/.ouija/worktrees/legacy-err");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(format!("{legacy}/SENTINEL"), "untouched").unwrap();

        let result = create_ouija_worktree(
            &repo_dir,
            "legacy-err",
            Some("any-branch"),
            Some("any-base"),
            /* force_reset = */ true,
            home.path(),
        );

        assert!(
            result.is_err(),
            "legacy path must return Err when force_reset=true cannot \
             be honored; got Ok({:?})",
            result.ok()
        );
        // Contents must still be untouched — the legacy dir is read-only
        // on this path regardless of the return shape.
        assert_eq!(
            std::fs::read_to_string(format!("{legacy}/SENTINEL")).unwrap(),
            "untouched",
            "legacy worktree contents must not be altered"
        );
    }
}
