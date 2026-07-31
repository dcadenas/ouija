mod admin;
mod api;
mod backend;
mod config;
pub mod daemon_protocol;
mod hooks;
mod nostr_transport;
mod persistence;
mod project_identity;
mod project_index;
mod protocol;
mod rollover;
mod router;
mod scheduler;
mod server;
mod session_agent;
mod state;
mod tmux;
mod tmux_var;
mod transport;

use anyhow::{Context, bail};
use backend::CodingAssistant;
use clap::{Parser, Subcommand, ValueEnum};
use daemon_protocol::IdlePolicy;
use nostr_sdk::ToBech32;
use std::io::Read;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "ouija", about = "Cross-machine AI session daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon
    #[command(name = "start-server")]
    StartServer {
        #[arg(short, long, default_value = "7880")]
        port: u16,
        #[arg(short, long)]
        name: Option<String>,
        #[arg(long)]
        data: Option<String>,
        /// Connect to a node using an nprofile1 ticket
        #[arg(long)]
        ticket: Option<String>,
        /// Additional nostr relay URLs (repeatable)
        #[arg(long = "relay")]
        relays: Vec<String>,
    },
    /// Show daemon status
    Status,
    /// List connected and saved nodes
    Nodes,
    /// Print connection ticket for this daemon
    Ticket {
        /// Additional relay URLs for ticket generation (repeatable)
        #[arg(long = "relay")]
        relays: Vec<String>,
    },
    /// Regenerate the connection ticket (invalidates the old one)
    RegenerateTicket {
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Connect to a node using a ticket
    Connect {
        ticket: String,
        /// Optional name to identify this node
        #[arg(long)]
        name: Option<String>,
    },
    /// Disconnect from a remote node
    Disconnect {
        /// Node name or daemon npub to disconnect
        node: String,
    },
    /// Register a session
    Register {
        id: String,
        pane: Option<String>,
        #[arg(long)]
        vim_mode: bool,
        #[arg(long)]
        project_dir: Option<String>,
        #[arg(long)]
        role: Option<String>,
    },
    /// Send a message expecting a reply
    Ask {
        to: String,
        message: Option<String>,
        /// Read message body from stdin.
        #[arg(long)]
        stdin: bool,
        /// Read message body from a file.
        #[arg(long)]
        message_file: Option<PathBuf>,
        /// Sender session ID: the exact output of `ouija whoami` (never a guessed id)
        #[arg(long)]
        from: Option<String>,
    },
    /// Send a message (fire-and-forget)
    Tell {
        to: String,
        message: Option<String>,
        /// Read message body from stdin.
        #[arg(long)]
        stdin: bool,
        /// Read message body from a file.
        #[arg(long)]
        message_file: Option<PathBuf>,
        /// Thread as progress update for a pending reply
        #[arg(long)]
        reply_to: Option<u64>,
        /// Sender session ID: the exact output of `ouija whoami` (never a guessed id)
        #[arg(long)]
        from: Option<String>,
    },
    /// Reply to a message (defaults to done=true)
    Reply {
        to: String,
        msg_id: u64,
        message: Option<String>,
        /// Read message body from stdin.
        #[arg(long)]
        stdin: bool,
        /// Read message body from a file.
        #[arg(long)]
        message_file: Option<PathBuf>,
        /// Don't mark as done (progress update)
        #[arg(long)]
        no_done: bool,
        /// Expect a reply back
        #[arg(long)]
        expect_reply: bool,
        /// Sender session ID: the exact output of `ouija whoami` (never a guessed id)
        #[arg(long)]
        from: Option<String>,
    },
    /// List sessions
    Ls,
    /// Print this session's Ouija id (same resolution path as ask/tell/reply)
    Whoami,
    /// Bind this running backend to one exact blank Local session without restarting it.
    #[command(name = "recover-backend-identity")]
    RecoverBackendIdentity {
        /// Exact public Local session ID supplied by the operator or injected context.
        session_id: String,
    },
    /// Prepare or adopt a session-owned context rollover.
    Rollover {
        #[command(subcommand)]
        action: RolloverAction,
    },
    /// Update session metadata
    Announce {
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        bulletin: Option<String>,
    },
    /// Inject directly into a tmux pane
    Inject { pane: String, message: String },
    /// Rename current session
    Rename {
        new_id: String,
        /// Session ID to rename: the exact Local id supplied by trusted context or the operator
        #[arg(long)]
        from: Option<String>,
    },
    /// Unregister a session (without killing it)
    Unregister { id: String },
    /// Start a new session.
    ///
    /// There is no `ouija spawn` alias. The initial `--prompt` is the complete
    /// bounded assignment; this command does not support `--one-shot-file`.
    #[command(name = "spawn-session")]
    SpawnSession {
        name: String,
        #[arg(long)]
        project_dir: Option<String>,
        /// Complete re-entrant, state-checking assignment stored as the session's base prompt and replayed after fresh restarts; verify live state and perform only remaining work. Before destructive or external actions, verify completion and current authorization.
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long, value_parser = parse_manual_reminder)]
        reminder: Option<String>,
        #[arg(long)]
        parent_session: Option<String>,
        #[arg(long)]
        no_parent_session: bool,
        /// What to do when work completes.
        #[arg(long, value_enum, conflicts_with = "idle_policy")]
        when_done: Option<WhenDone>,
        /// Deprecated: use --when-done. Legacy values: keep-open, ask-parent-when-done, close-when-done.
        #[arg(
            long,
            value_parser = parse_idle_policy,
            conflicts_with = "when_done"
        )]
        idle_policy: Option<IdlePolicy>,
        #[arg(long)]
        worktree: bool,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        base_branch: Option<String>,
        /// LLM model (claude: alias/full id; opencode: providerID/modelID).
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort / variant (claude: --effort; codex: model_reasoning_effort; opencode: prompt variant).
        #[arg(long)]
        effort: Option<String>,
        /// Bootstrap active-context refresh after this much accumulated active work (e.g. 1h, 90m, 3600s).
        #[arg(long, value_parser = parse_fresh_context_after_active)]
        fresh_context_after_active: Option<u64>,
        #[arg(long)]
        backend: Option<String>,
        #[arg(long)]
        from: Option<String>,
    },
    /// Kill a running session
    #[command(name = "kill-session")]
    KillSession {
        name: String,
        #[arg(long)]
        keep_worktree: bool,
    },
    /// Prune stale sessions whose worktree is missing
    #[command(name = "prune-stale-sessions")]
    PruneStaleSessions {
        /// Actually remove (default is dry-run)
        #[arg(long, short)]
        yes: bool,
    },
    /// Restart a session
    #[command(name = "restart-session")]
    RestartSession {
        name: String,
        #[arg(long)]
        fresh: bool,
        /// Set or change active-context refresh after this much accumulated active work.
        #[arg(
            long,
            requires = "fresh",
            value_parser = parse_fresh_context_after_active
        )]
        fresh_context_after_active: Option<u64>,
        /// Replace the durable stored base prompt when absent or transient recovery prose. It is replayed by default after every fresh restart; make it re-entrant, state-checking, guard expensive, destructive, or external actions against repetition, and verify current authorization before destructive or external actions.
        #[arg(long)]
        prompt: Option<String>,
        /// Do not reuse the stored startup prompt for this launch.
        #[arg(long)]
        suppress_stored_prompt: bool,
        /// Append a verified current-work continuation for this launch only.
        #[arg(long)]
        one_shot_file: Option<PathBuf>,
        #[arg(long, value_parser = parse_manual_reminder)]
        reminder: Option<String>,
        /// Select the backend explicitly when its binding is absent or cannot be trusted.
        #[arg(long)]
        backend: Option<String>,
        /// Override the LLM model on restart (defaults to the previous model).
        #[arg(long)]
        model: Option<String>,
        /// Override the reasoning effort on restart (defaults to the previous effort).
        #[arg(long)]
        effort: Option<String>,
    },
    /// Clear an idle reminder
    #[command(name = "clear-reminder")]
    ClearReminder {
        clearing_id: u64,
        /// Sender session ID: the exact output of `ouija whoami` (never a guessed id)
        #[arg(long)]
        from: Option<String>,
    },
    /// Clear a pending reply from a disconnected sender
    #[command(name = "clear-reply")]
    ClearReply { sender_id: String },
    /// Stop the running daemon
    #[command(name = "stop-server")]
    StopServer,
    /// Restart the running daemon
    #[command(name = "restart-server")]
    RestartServer,
    /// Print the message log file path
    LogPath {
        #[arg(long)]
        data: Option<String>,
    },
    /// Update ouija from crates.io and restart daemon
    #[command(name = "self-update")]
    SelfUpdate,
    /// View or change daemon settings
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Manage scheduled tasks
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
}

#[derive(Subcommand)]
enum RolloverAction {
    /// Store a bounded continuation for a future incarnation.
    Prepare {
        /// Read the continuation JSON from stdin.
        #[arg(long, required = true)]
        stdin: bool,
        /// Replace a pending record only when it is already expired.
        #[arg(long)]
        replace_expired: bool,
    },
    /// Verify and adopt a prepared continuation.
    Adopt { token: String },
    /// Remove an adopted or expired continuation.
    Cleanup {
        /// Also remove a live pending continuation.
        #[arg(long)]
        force_pending: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Set a config value: default_backend (claude-code, opencode, codex-cli), auto_register, etc.
    Set { key: String, value: String },
    /// Route a Codex model alias to backend-specific launch config
    SetCodexModelRoute {
        /// User-facing model alias, e.g. gemini
        alias: String,
        /// Actual model passed to Codex for this alias
        #[arg(long)]
        model: Option<String>,
        /// Codex home containing the provider configuration for this alias
        #[arg(long)]
        codex_home: Option<String>,
    },
    /// Remove a Codex model alias route
    RemoveCodexModelRoute {
        /// User-facing model alias to remove
        alias: String,
    },
    /// Add a Nostr DM user (human who can control the daemon via DMs)
    AddHuman {
        /// The user's Nostr public key (npub1...)
        #[arg(long)]
        npub: String,
        /// Display name for this user
        #[arg(long)]
        name: String,
        /// Default session to route unprefixed messages to
        #[arg(long)]
        default_session: Option<String>,
    },
    /// Remove a Nostr DM user
    RemoveHuman {
        /// Name of the user to remove
        #[arg(long)]
        name: String,
    },
    /// List configured Nostr DM users
    ListHumans,
    /// Configure the LLM router for human DMs
    SetRouter {
        /// Anthropic API key (falls back to ANTHROPIC_API_KEY env var if omitted)
        #[arg(long)]
        api_key: Option<String>,
        /// Model to use (default: claude-haiku-4-5-20251001)
        #[arg(long)]
        model: Option<String>,
        /// Base URL (default: https://api.anthropic.com)
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Remove LLM router configuration
    RemoveRouter,
}

#[derive(Subcommand)]
enum TaskAction {
    /// List all scheduled tasks
    List,
    /// Add a new scheduled task (cron in UTC)
    Add {
        name: String,
        /// Cron expression (e.g. "*/5 * * * *"), evaluated in UTC
        cron: String,
        /// Message to inject
        message: String,
        /// Inject into this existing session (continue_session mode only)
        #[arg(long)]
        target: Option<String>,
        /// Inject only into the exact currently live Local target; never revive it
        #[arg(long, requires = "target")]
        inject_only: bool,
        /// Override project dir for session revival
        #[arg(long)]
        project_dir: Option<String>,
        /// Backend used when creating/reviving the task session
        #[arg(long)]
        backend: Option<String>,
        /// LLM model override used when creating/reviving the task session
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort / variant used when creating/reviving the task session
        #[arg(long)]
        effort: Option<String>,
        /// Fire once then auto-delete
        #[arg(long)]
        once: bool,
    },
    /// Remove a scheduled task
    Remove { id: String },
    /// Enable a disabled task
    Enable { id: String },
    /// Disable a task
    Disable { id: String },
    /// Show recent task executions
    Runs {
        /// Filter by task ID
        #[arg(long)]
        task: Option<String>,
    },
    /// Manually trigger a task now
    Trigger { id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, ValueEnum)]
enum WhenDone {
    KeepOpen,
    AskParent,
    Close,
}

impl From<WhenDone> for IdlePolicy {
    fn from(value: WhenDone) -> Self {
        match value {
            WhenDone::KeepOpen => IdlePolicy::KeepOpen,
            WhenDone::AskParent => IdlePolicy::AskParentWhenDone,
            WhenDone::Close => IdlePolicy::CloseWhenDone,
        }
    }
}

fn parse_idle_policy(value: &str) -> Result<IdlePolicy, String> {
    value.parse()
}

fn parse_manual_reminder(value: &str) -> Result<String, String> {
    daemon_protocol::validate_spawn_reminder(Some(value))?;
    Ok(value.to_string())
}

fn parse_fresh_context_after_active(value: &str) -> Result<u64, String> {
    let Some(unit) = value.chars().last() else {
        return Err("duration must be a positive whole number followed by h, m, or s".into());
    };
    let multiplier = match unit {
        'h' => 3_600,
        'm' => 60,
        's' => 1,
        _ => return Err("duration must end with h, m, or s".into()),
    };
    let amount = &value[..value.len() - unit.len_utf8()];
    if amount.is_empty() || !amount.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("duration must be a positive whole number followed by h, m, or s".into());
    }
    let amount = amount
        .parse::<u64>()
        .map_err(|_| "duration value is too large".to_string())?;
    if amount == 0 {
        return Err("duration must be greater than zero".into());
    }
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| "duration overflows seconds".to_string())
}

fn validate_spawn_lifecycle(
    parent_session: Option<&str>,
    no_parent_session: bool,
    idle_policy: Option<&IdlePolicy>,
) -> Result<(), String> {
    daemon_protocol::validate_spawn_lifecycle(parent_session, no_parent_session, idle_policy)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install rustls CryptoProvider before any TLS connections (nostr, reqwest).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls CryptoProvider");

    let cli = Cli::parse();

    // Daemon logs to a file in the data dir; CLI subcommands log to stderr.
    if !matches!(cli.command, Command::StartServer { .. }) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "ouija=warn".parse().expect("valid default filter")),
            )
            .init();
    }

    match cli.command {
        Command::StartServer {
            port,
            name,
            data,
            ticket,
            relays,
        } => {
            // Compute data dir early so we can point tracing at it.
            let data_dir = match data.as_deref() {
                Some(d) => std::path::PathBuf::from(d),
                None => config::OuijaConfig::default_data_dir(),
            };
            std::fs::create_dir_all(&data_dir)?;

            let log_file = std::fs::File::create(data_dir.join("daemon.log"))?;
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "ouija=info".parse().expect("valid default filter")),
                )
                .with_writer(log_file)
                .with_ansi(false)
                .init();

            preflight_checks();
            let _ = backend::claude_code::ClaudeCode.install();
            let _ = backend::opencode::OpenCode.install();
            let _ = backend::codex::Codex.install();

            let name = name.unwrap_or_else(|| {
                hostname::get()
                    .ok()
                    .and_then(|h| h.into_string().ok())
                    .unwrap_or_else(|| "ouija".to_string())
            });

            // Load nostr keys early — the npub serves as the daemon's universal identity.
            // Config dir holds the nsec; data dir holds runtime state.
            let config_dir = match data.as_deref() {
                Some(d) => std::path::PathBuf::from(d),
                None => config::OuijaConfig::default_config_dir(),
            };
            std::fs::create_dir_all(&config_dir)?;
            let nostr_keys = nostr_transport::load_or_create_keys(&config_dir)?;
            let npub = nostr_keys
                .public_key()
                .to_bech32()
                .unwrap_or_else(|_| "unknown".into());
            tracing::info!("daemon identity: {npub}");

            {
                let registry = backend::BackendRegistry::default_registry();
                let available = registry.available();
                if available.is_empty() {
                    eprintln!(
                        "error: no coding backend found in PATH. Install claude-code, opencode, or codex.\n\
                         See: https://docs.anthropic.com/en/docs/claude-code\n\
                         See: https://opencode.ai\n\
                         See: https://developers.openai.com/codex"
                    );
                    std::process::exit(1);
                }
                tracing::info!("available backends: {}", available.join(", "));
            }

            let config = config::OuijaConfig::new(name, port, data, npub)?;
            let state = state::AppState::new(config);
            if let Some(home) = state.settings.read().await.codex_home.clone() {
                backend::codex::install_configured_home(Some(&home));
            }
            {
                let route_homes: Vec<String> = state
                    .settings
                    .read()
                    .await
                    .codex_model_routes
                    .values()
                    .filter_map(|route| route.codex_home.clone())
                    .collect();
                for home in route_homes {
                    backend::codex::install_configured_home(Some(&home));
                }
            }

            // Build project index in background
            let index_state = state.clone();
            tokio::spawn(async move {
                project_index::refresh_index(&index_state).await;
            });

            // Restore persisted sessions synchronously before the reaper loop
            // starts, so auto-register doesn't overwrite custom names.
            restore_persisted_sessions(&state).await?;
            register_human_sessions(&state).await;

            // Setup nostr transport in the background so HTTP starts immediately.
            let bg_state = state.clone();
            tokio::spawn(async move {
                setup_nostr_transport(&bg_state, ticket.as_deref(), relays).await;
            });

            // Reap dead sessions, auto-register, and broadcast on change
            let reaper_state = state.clone();
            tokio::spawn(async move {
                let mut last_session_hash: u64 = 0;
                let mut first_run = true;
                let mut heartbeat_counter: u64 = 0;
                // Re-announce every HEARTBEAT_CYCLES reaper ticks (~60s at default 10s interval)
                const HEARTBEAT_CYCLES: u64 = 6;

                loop {
                    let interval = reaper_state.settings.read().await.reaper_interval_secs;
                    tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

                    // Reap dead local sessions via protocol
                    let panes_to_check: Vec<(crate::daemon_protocol::ResourceOwner, String)> = {
                        let proto = reaper_state.protocol.read().await;
                        let now = chrono::Utc::now().timestamp();
                        proto
                            .sessions
                            .values()
                            .filter(|s| {
                                matches!(s.origin, crate::daemon_protocol::Origin::Local)
                                    && s.pane.is_some()
                                    && (s.registered_at == 0 || now - s.registered_at > 60)
                                    // HTTP-delivered sessions (opencode shared serve)
                                    // are reachable independently of the tmux pane, so a
                                    // dead/absent attach TUI must not get them reaped.
                                    && !s.metadata.backend.as_deref().is_some_and(|b| {
                                        reaper_state.backends.uses_http_delivery(b)
                                    })
                            })
                            .filter_map(|s| Some((s.owner(), s.pane.clone()?)))
                            .collect()
                    };
                    let dead_sessions: Vec<(crate::daemon_protocol::ResourceOwner, String)> =
                        if !panes_to_check.is_empty() {
                            let names: Vec<String> = reaper_state.backends.all_process_names();
                            let dead = tokio::task::spawn_blocking(move || {
                                let name_refs: Vec<&str> =
                                    names.iter().map(|s| s.as_str()).collect();
                                panes_to_check
                                    .into_iter()
                                    .filter(|(_, pane)| !crate::tmux::pane_alive(pane, &name_refs))
                                    .collect::<Vec<_>>()
                            })
                            .await
                            .unwrap_or_default();
                            if !dead.is_empty() {
                                reaper_state
                                    .apply_and_execute(crate::daemon_protocol::Event::ReapDead {
                                        dead_sessions: dead.clone(),
                                    })
                                    .await;
                            }
                            dead
                        } else {
                            vec![]
                        };
                    // Clean up per-fire worktree panes
                    let perfire_to_check: Vec<(String, crate::state::PerFireWorktreeClaim)> = {
                        let pf = reaper_state.perfire_worktree_panes.read().await;
                        pf.iter().map(|(p, d)| (p.clone(), d.clone())).collect()
                    };
                    if !perfire_to_check.is_empty() {
                        let names: Vec<String> = reaper_state.backends.all_process_names();
                        let dead_perfire = tokio::task::spawn_blocking(move || {
                            let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
                            perfire_to_check
                                .into_iter()
                                .filter(|(pane, _)| !crate::tmux::pane_alive(pane, &name_refs))
                                .collect::<Vec<_>>()
                        })
                        .await
                        .unwrap_or_default();
                        if !dead_perfire.is_empty() {
                            for (pane_id, claim) in dead_perfire {
                                tracing::info!(
                                    "per-fire worktree pane {pane_id} died, pruning worktrees in {}",
                                    claim.project_dir
                                );
                                reaper_state
                                    .prune_dead_perfire_worktree(&pane_id, &claim)
                                    .await;
                            }
                        }
                    }
                    let _ = dead_sessions; // suppress unused warning

                    // If over the max session limit, close the most idle ones.
                    // Killing the pane lets the next reaper cycle clean up + broadcast.
                    for (owner, pane) in reaper_state.collect_excess_idle_sessions().await {
                        let id = owner.session_id.clone();
                        tracing::info!(
                            "auto-closing idle session '{id}' (over max_local_sessions)"
                        );
                        crate::nostr_transport::kill_session_owned(&reaper_state, &owner, &pane)
                            .await;
                    }

                    // Scan tmux, update cache, auto-register unregistered panes
                    reaper_state.scan_and_autoregister_panes().await;

                    // Broadcast full session list on startup, when it changes,
                    // or periodically as a heartbeat so peers reconnect after
                    // relay disconnections or daemon restarts.
                    heartbeat_counter += 1;
                    let current_hash = reaper_state.local_session_hash().await;
                    let heartbeat_due = heartbeat_counter >= HEARTBEAT_CYCLES;
                    if first_run || current_hash != last_session_hash || heartbeat_due {
                        // Initial sweep on startup + periodic on heartbeat cadence
                        if first_run || heartbeat_due {
                            reaper_state.sweep_worktree_presence().await;
                        }
                        transport::broadcast_local_sessions(&reaper_state).await;
                        last_session_hash = current_hash;
                        first_run = false;
                        if heartbeat_due {
                            heartbeat_counter = 0;
                        }
                    }
                }
            });

            // Run scheduler loop for periodic tasks
            let scheduler_state = state.clone();
            tokio::spawn(crate::scheduler::run_scheduler(scheduler_state));

            server::run(state).await?;
        }
        Command::Status => {
            cli_get("/api/status").await?;
        }
        Command::Nodes => {
            let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
            let url = format!("http://localhost:{port}/api/nodes");
            let resp: serde_json::Value = reqwest::get(&url).await?.json().await?;
            let nodes = resp["nodes"].as_array();
            match nodes {
                Some(list) if !list.is_empty() => {
                    println!("{:<16} {:<12} {:<20} SINCE", "NAME", "STATUS", "NPUB");
                    for p in list {
                        let name = p["name"].as_str().unwrap_or("-");
                        let status = p["status"].as_str().unwrap_or("unknown");
                        let npub = p["npub"].as_str().unwrap_or("-");
                        let npub_short = if npub.len() > 20 {
                            format!("{}…{}", &npub[..10], &npub[npub.len() - 6..])
                        } else {
                            npub.to_string()
                        };
                        let since = p["since"].as_str().unwrap_or("-");
                        println!("{:<16} {:<12} {:<20} {}", name, status, npub_short, since);
                    }
                }
                _ => println!("no nodes"),
            }
        }
        Command::Ticket { relays } => {
            let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
            let url = format!("http://localhost:{port}/api/ticket");
            let client = reqwest::Client::new();
            let mut req = client.get(&url);
            for r in &relays {
                req = req.query(&[("relay", r.as_str())]);
            }
            let resp: serde_json::Value = req.send().await?.json().await?;
            if let Some(ticket) = resp["ticket"].as_str() {
                println!("{ticket}");
            } else if let Some(err) = resp["error"].as_str() {
                eprintln!("error: {err}");
                std::process::exit(1);
            } else {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        }
        Command::RegenerateTicket { yes } => {
            if !yes {
                eprintln!(
                    "WARNING: This will destroy your nostr identity (nsec). All nodes must re-connect."
                );
                eprintln!("Run with --yes to confirm.");
                std::process::exit(1);
            }
            let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
            let url = format!("http://localhost:{port}/api/regenerate-ticket?confirm=true");
            let client = reqwest::Client::new();
            let resp: serde_json::Value = client.post(&url).send().await?.json().await?;
            if let Some(ticket) = resp["ticket"].as_str() {
                println!("{ticket}");
            } else if let Some(err) = resp["error"].as_str() {
                eprintln!("Error: {err}");
                std::process::exit(1);
            }
        }
        Command::Connect { ticket, name } => {
            let body = serde_json::json!({ "ticket": ticket, "name": name });
            cli_post("/api/connect", &body).await?;
        }
        Command::Disconnect { node } => {
            // Resolve node name to daemon_id (npub) via the nodes API
            let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
            let url = format!("http://localhost:{port}/api/status");
            let resp: serde_json::Value = reqwest::get(&url).await?.json().await?;
            let daemon_id = resp["nodes"].as_array().and_then(|nodes| {
                nodes.iter().find_map(|n| {
                    let name = n["name"].as_str().unwrap_or("");
                    let did = n["daemon_id"].as_str().unwrap_or("");
                    if name == node || did == node {
                        Some(did.to_string())
                    } else {
                        None
                    }
                })
            });
            match daemon_id {
                Some(id) => {
                    let body = serde_json::json!({ "daemon_id": id });
                    cli_post("/api/nodes/disconnect", &body).await?;
                }
                None => {
                    // Try as a raw daemon_id (npub) directly
                    let body = serde_json::json!({ "daemon_id": node });
                    cli_post("/api/nodes/disconnect", &body).await?;
                }
            }
        }
        Command::Register {
            id,
            pane,
            vim_mode,
            project_dir,
            role,
        } => {
            let pane = pane.or_else(|| std::env::var("TMUX_PANE").ok());
            let body = serde_json::json!({
                "id": id,
                "pane": pane,
                "vim_mode": vim_mode,
                "project_dir": project_dir,
                "role": role,
            });
            cli_post("/api/register", &body).await?;
        }
        Command::Ask {
            to,
            message,
            stdin,
            message_file,
            from,
        } => {
            let message = resolve_message(message, stdin, message_file)?;
            let sender = resolve_sender(from).await?;
            let body = serde_json::json!({
                "from": sender.id,
                "to": to,
                "message": message,
                "expects_reply": true,
                "sender_ctx": sender.context,
            });
            cli_post("/api/send", &body).await?;
        }
        Command::Tell {
            to,
            message,
            stdin,
            message_file,
            reply_to,
            from,
        } => {
            let message = resolve_message(message, stdin, message_file)?;
            let sender = resolve_sender(from).await?;
            let body = serde_json::json!({
                "from": sender.id,
                "to": to,
                "message": message,
                "expects_reply": false,
                "responds_to": reply_to,
                "sender_ctx": sender.context,
            });
            cli_post("/api/send", &body).await?;
        }
        Command::Reply {
            to,
            msg_id,
            message,
            stdin,
            message_file,
            no_done,
            expect_reply,
            from,
        } => {
            let message = resolve_message(message, stdin, message_file)?;
            let sender = resolve_sender(from).await?;
            let body = serde_json::json!({
                "from": sender.id,
                "to": to,
                "message": message,
                "expects_reply": expect_reply,
                "responds_to": msg_id,
                "done": !no_done,
                "sender_ctx": sender.context,
            });
            cli_post("/api/send", &body).await?;
        }
        Command::Ls => {
            cli_list_sessions().await?;
        }
        Command::Whoami => {
            cli_whoami().await?;
        }
        Command::RecoverBackendIdentity { session_id } => {
            let identity = backend::BackendRegistry::default_registry()
                .caller_session_identity()
                .context(
                    "recovery requires exactly one complete backend identity from the current adapter",
                )?;
            let tmux_pane = std::env::var("TMUX_PANE")
                .ok()
                .filter(|pane| !pane.is_empty());
            let pane_var = tmux_pane.as_deref().and_then(tmux_var::get);
            let env_var = std::env::var("OUIJA_SESSION_ID")
                .ok()
                .filter(|id| !id.is_empty());
            let caller = backend_recovery_caller_evidence(tmux_pane, pane_var, env_var);
            let body = serde_json::json!({
                "target_session_id": session_id,
                "identity": identity,
                "caller": caller,
            });
            cli_post("/api/backend-identities/recover", &body).await?;
        }
        Command::Rollover { action } => {
            let caller = rollover_live_caller().await?;
            let data_dir = config::OuijaConfig::default_data_dir();
            let now = chrono::Utc::now().timestamp();
            match action {
                RolloverAction::Prepare {
                    stdin: _,
                    replace_expired,
                } => {
                    let mut input = Vec::new();
                    std::io::Read::read_to_end(
                        &mut std::io::stdin().take(16 * 1024 + 1),
                        &mut input,
                    )
                    .context("failed to read continuation JSON from stdin")?;
                    let payload = rollover::parse_continuation(&input)?;
                    let token =
                        rollover::prepare(&data_dir, &caller, payload, replace_expired, now)?;
                    println!("{token}");
                }
                RolloverAction::Adopt { token } => {
                    let payload = rollover::adopt(&data_dir, &caller, &token, now)?;
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                }
                RolloverAction::Cleanup { force_pending } => {
                    if rollover::cleanup(&data_dir, &caller, force_pending, now)? {
                        println!("continuation removed");
                    } else {
                        println!("no continuation for this session");
                    }
                }
            }
        }
        Command::Announce { role, bulletin } => {
            if role.is_none() && bulletin.is_none() {
                anyhow::bail!("at least one of --role or --bulletin is required");
            }
            let id = require_my_session_id().await?;
            let body = serde_json::json!({
                "id": id,
                "role": role,
                "bulletin": bulletin,
            });
            cli_post("/api/sessions/update", &body).await?;
        }
        Command::Inject { pane, message } => {
            let body = serde_json::json!({ "pane": pane, "message": message });
            cli_post("/api/inject", &body).await?;
        }
        Command::Rename { new_id, from } => {
            let sender = resolve_sender(from).await?;
            let body = serde_json::json!({
                "old_id": sender.id,
                "new_id": new_id,
                "sender_ctx": sender.context,
            });
            cli_post("/api/rename", &body).await?;
        }
        Command::Unregister { id } => {
            let body = serde_json::json!({ "id": id });
            cli_post("/api/remove", &body).await?;
        }
        Command::SpawnSession {
            name,
            project_dir,
            prompt,
            reminder,
            parent_session,
            no_parent_session,
            when_done,
            idle_policy,
            worktree,
            branch,
            base_branch,
            model,
            effort,
            fresh_context_after_active,
            backend,
            from,
        } => {
            let idle_policy = when_done.map(IdlePolicy::from).or(idle_policy);
            if let Err(err) = validate_spawn_lifecycle(
                parent_session.as_deref(),
                no_parent_session,
                idle_policy.as_ref(),
            ) {
                anyhow::bail!("{err}");
            }
            let body = serde_json::json!({
                "name": name,
                "project_dir": project_dir,
                "prompt": prompt,
                "reminder": reminder,
                "parent_session": parent_session,
                "no_parent_session": no_parent_session,
                "idle_policy": idle_policy,
                "worktree": worktree,
                "branch": branch,
                "base_branch": base_branch,
                "model": model,
                "effort": effort,
                "fresh_context_after_active_secs": fresh_context_after_active,
                "backend": backend,
                "from": from,
            });
            cli_post("/api/sessions/start", &body).await?;
        }
        Command::KillSession {
            name,
            keep_worktree,
        } => {
            let body = serde_json::json!({
                "name": name,
                "keep_worktree": keep_worktree,
            });
            cli_post("/api/sessions/kill", &body).await?;
        }
        Command::PruneStaleSessions { yes } => {
            let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
            let url = format!("http://localhost:{port}/api/sessions/prune-stale");
            let client = reqwest::Client::new();
            let body_json = serde_json::json!({ "confirm": yes });
            let mut resp = client.post(&url).json(&body_json).send().await?;
            resp = resp.error_for_status()?;
            let text = resp.text().await?;
            let value: serde_json::Value = serde_json::from_str(&text)?;

            // Require dry_run key presence to detect schema drift / empty response bugs
            let dry_run = value
                .get("dry_run")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| anyhow::anyhow!("server response missing 'dry_run' key: {text}"))?;

            if dry_run == yes {
                return Err(anyhow::anyhow!(
                    "server response intent mismatch: requested confirm={} but server returned dry_run={}. Response: {}",
                    yes,
                    dry_run,
                    text
                ));
            } else if dry_run {
                // Would prune branch (dry_run=true requested, yes=false)
                // Require would_prune key on dry-run
                if let Some(arr) = value.get("would_prune").and_then(|v| v.as_array()) {
                    let ids = arr.len();
                    if ids == 0 {
                        println!("No stale sessions to prune");
                    } else {
                        println!(
                            "Would prune {} stale session(s): {}",
                            ids, value["would_prune"]
                        );
                        println!("Run with --yes to confirm removal");
                    }
                } else {
                    return Err(anyhow::anyhow!(
                        "server response missing 'would_prune' key on dry_run=true: {text}"
                    ));
                }
            } else {
                // Require pruned key on confirm; exit non-zero on errors for scripting
                let arr = value
                    .get("pruned")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "server response missing 'pruned' key on confirm=true: {text}"
                        )
                    })?;
                println!("Pruned {} stale session(s)", arr.len());

                // Check for errors key with proper array shape; fail on schema drift
                if value.get("errors").is_some() {
                    let err_arr =
                        value
                            .get("errors")
                            .and_then(|v| v.as_array())
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "server response 'errors' key is not an array: {text}"
                                )
                            })?;
                    eprintln!(
                        "Failed to prune {} session(s): {}",
                        err_arr.len(),
                        value["errors"]
                    );
                    if !err_arr.is_empty() {
                        return Err(anyhow::anyhow!(
                            "partial failure: {} session(s) failed to prune",
                            err_arr.len()
                        ));
                    }
                }

                // Check for already_gone key - sessions that vanished during prune
                if value.get("already_gone").is_some() {
                    let gone_arr = value
                        .get("already_gone")
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "server response 'already_gone' key is not an array: {text}"
                            )
                        })?;
                    if !gone_arr.is_empty() {
                        eprintln!(
                            "Skipped {} session(s) that vanished during prune: {}",
                            gone_arr.len(),
                            value["already_gone"]
                        );
                    }
                }
            }
        }
        Command::RestartSession {
            name,
            fresh,
            fresh_context_after_active,
            prompt,
            suppress_stored_prompt,
            one_shot_file,
            reminder,
            backend,
            model,
            effort,
        } => {
            let one_shot_prompt = one_shot_file
                .as_deref()
                .map(read_one_shot_file)
                .transpose()?;
            let body = serde_json::json!({
                "name": name,
                "fresh": fresh,
                "fresh_context_after_active_secs": fresh_context_after_active,
                "prompt": prompt,
                "suppress_stored_prompt": suppress_stored_prompt,
                "one_shot_prompt": one_shot_prompt,
                "reminder": reminder,
                "backend": backend,
                "model": model,
                "effort": effort,
            });
            cli_post("/api/sessions/restart", &body).await?;
        }
        Command::ClearReminder { clearing_id, from } => {
            let from = match from {
                Some(id) => id,
                None => require_my_session_id().await?,
            };
            let body = serde_json::json!({
                "from": from,
                "clearing_id": clearing_id,
            });
            cli_post("/api/clear-reminder", &body).await?;
        }
        Command::ClearReply { sender_id } => {
            let pane = std::env::var("TMUX_PANE")
                .context("TMUX_PANE not set — must be run from a tmux pane")?;
            // Strip the leading `%` — axum percent-decodes `%74` to `t` and
            // would silently 404. See `pane_wire_suffix` docstring and #646.
            let pane = pane_wire_suffix(&pane);
            // Percent-encode sender_id: ouija session ids can contain `/`
            // (branch-name-style ids from `/api/sessions/start`), which would
            // otherwise break axum's single-segment match on `{from}` and
            // silently 404. See `encode_path_segment` docstring.
            let sender_id = encode_path_segment(&sender_id);
            cli_delete(&format!("/api/pane/{pane}/pending-replies/{sender_id}")).await?;
        }
        Command::StopServer => {
            stop_daemon()?;
        }
        Command::RestartServer => {
            // systemd/legacy-aware restart, so callers (e.g. the use-published
            // task) never have to start the foreground `start-server` directly.
            restart_daemon()?;
            let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
            let status_url = format!("http://localhost:{port}/api/status");
            if wait_for_daemon(&status_url) {
                println!("daemon restarted");
            } else {
                anyhow::bail!("daemon did not come back within 10s");
            }
        }
        Command::LogPath { data } => {
            let config = config::OuijaConfig::new("_".into(), 0, data, String::new())?;
            println!("{}", config.data_dir.join("messages.jsonl").display());
            println!("{}", config.data_dir.join("daemon.log").display());
        }
        Command::SelfUpdate => {
            update_and_restart()?;
        }
        Command::Config { action } => match action {
            None => cli_get("/api/settings").await?,
            Some(ConfigAction::Set { key, value }) => {
                let parsed: serde_json::Value = match value.as_str() {
                    "true" => serde_json::Value::Bool(true),
                    "false" => serde_json::Value::Bool(false),
                    v => serde_json::Value::String(v.to_string()),
                };
                let body = serde_json::json!({ key: parsed });
                cli_post("/api/settings", &body).await?;
            }
            Some(ConfigAction::SetCodexModelRoute {
                alias,
                model,
                codex_home,
            }) => {
                if alias.trim().is_empty() {
                    anyhow::bail!("alias cannot be empty");
                }
                let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
                let url = format!("http://localhost:{port}/api/settings");
                let current: serde_json::Value = reqwest::get(&url).await?.json().await?;
                let mut routes = current
                    .get("codex_model_routes")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                if !routes.is_object() {
                    routes = serde_json::json!({});
                }
                routes[alias.trim()] = serde_json::json!({
                    "model": model,
                    "codex_home": codex_home,
                });
                cli_post(
                    "/api/settings",
                    &serde_json::json!({
                        "codex_model_routes": routes,
                    }),
                )
                .await?;
            }
            Some(ConfigAction::RemoveCodexModelRoute { alias }) => {
                let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
                let url = format!("http://localhost:{port}/api/settings");
                let current: serde_json::Value = reqwest::get(&url).await?.json().await?;
                let mut routes = current
                    .get("codex_model_routes")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                if let Some(map) = routes.as_object_mut() {
                    map.remove(alias.trim());
                }
                cli_post(
                    "/api/settings",
                    &serde_json::json!({
                        "codex_model_routes": routes,
                    }),
                )
                .await?;
            }
            Some(ConfigAction::AddHuman {
                npub,
                name,
                default_session,
            }) => {
                if !npub.starts_with("npub1") {
                    anyhow::bail!("npub must start with 'npub1'");
                }
                let config_dir = config::OuijaConfig::default_config_dir();
                std::fs::create_dir_all(&config_dir)?;
                let mut settings = persistence::load_settings(&config_dir)?;
                if settings.human_sessions.iter().any(|h| h.name == name) {
                    anyhow::bail!("Nostr DM user '{name}' already exists");
                }
                settings.human_sessions.push(persistence::HumanSession {
                    npub,
                    name: name.clone(),
                    default_session,
                    welcomed: false,
                });
                persistence::save_settings(&config_dir, &settings)?;
                println!("added Nostr DM user '{name}'");
            }
            Some(ConfigAction::RemoveHuman { name }) => {
                let config_dir = config::OuijaConfig::default_config_dir();
                let mut settings = persistence::load_settings(&config_dir)?;
                let before = settings.human_sessions.len();
                settings.human_sessions.retain(|h| h.name != name);
                if settings.human_sessions.len() == before {
                    anyhow::bail!("Nostr DM user '{name}' not found");
                }
                persistence::save_settings(&config_dir, &settings)?;
                println!("removed Nostr DM user '{name}'");
            }
            Some(ConfigAction::ListHumans) => {
                let config_dir = config::OuijaConfig::default_config_dir();
                let settings = persistence::load_settings(&config_dir)?;
                if settings.human_sessions.is_empty() {
                    println!("no Nostr DM users configured");
                } else {
                    println!("{:<12} {:<20} DEFAULT", "NAME", "NPUB");
                    for h in &settings.human_sessions {
                        let npub_short = if h.npub.len() > 16 {
                            format!("{}...", &h.npub[..16])
                        } else {
                            h.npub.clone()
                        };
                        let default = h.default_session.as_deref().unwrap_or("--");
                        println!("{:<12} {:<20} {}", h.name, npub_short, default);
                    }
                }
            }
            Some(ConfigAction::SetRouter {
                api_key,
                model,
                base_url,
            }) => {
                let config_dir = config::OuijaConfig::default_config_dir();
                std::fs::create_dir_all(&config_dir)?;
                let mut settings = persistence::load_settings(&config_dir)?;
                settings.router = Some(persistence::RouterConfig {
                    api_key,
                    model: model.unwrap_or_else(|| "gemini-2.5-flash".to_string()),
                    base_url: base_url.unwrap_or_else(|| {
                        "https://generativelanguage.googleapis.com/v1beta/openai".to_string()
                    }),
                });
                persistence::save_settings(&config_dir, &settings)?;
                println!("router configured");
            }
            Some(ConfigAction::RemoveRouter) => {
                let config_dir = config::OuijaConfig::default_config_dir();
                let mut settings = persistence::load_settings(&config_dir)?;
                settings.router = None;
                persistence::save_settings(&config_dir, &settings)?;
                println!("router removed");
            }
        },
        Command::Task { action } => match action {
            TaskAction::List => {
                let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
                let url = format!("http://localhost:{port}/api/tasks");
                let resp: serde_json::Value = reqwest::get(&url).await?.json().await?;
                let tasks = resp["tasks"].as_array();
                match tasks {
                    Some(list) if !list.is_empty() => {
                        println!(
                            "{:<10} {:<16} {:<16} {:<12} {:<10} {:<10} {:<12} {:<8} {:<20} RUNS",
                            "ID",
                            "NAME",
                            "CRON",
                            "MODE",
                            "TARGET",
                            "BACKEND",
                            "MODEL",
                            "ENABLED",
                            "NEXT RUN"
                        );
                        for t in list {
                            let id = t["id"].as_str().unwrap_or("-");
                            let name = t["name"].as_str().unwrap_or("-");
                            let cron = t["cron"].as_str().unwrap_or("-");
                            let mode = t["on_fire"]["mode"].as_str().unwrap_or("-");
                            let target = t["target_session"].as_str().unwrap_or("—");
                            let backend = t["backend"].as_str().unwrap_or("—");
                            let model = t["model"].as_str().unwrap_or("—");
                            let enabled = t["enabled"].as_bool().unwrap_or(false);
                            let next = t["next_run"].as_str().unwrap_or("-");
                            let runs = t["run_count"].as_u64().unwrap_or(0);
                            println!(
                                "{:<10} {:<16} {:<16} {:<12} {:<10} {:<10} {:<12} {:<8} {:<20} {}",
                                id, name, cron, mode, target, backend, model, enabled, next, runs
                            );
                        }
                    }
                    _ => println!("no scheduled tasks"),
                }
            }
            TaskAction::Add {
                name,
                cron,
                target,
                message,
                inject_only,
                project_dir,
                backend,
                model,
                effort,
                once,
            } => {
                let body = serde_json::json!({
                    "name": name,
                    "cron": cron,
                    "target_session": target,
                    "message": message,
                    "on_fire": inject_only.then_some(crate::scheduler::OnFire::InjectOnly),
                    "project_dir": project_dir,
                    "backend": backend,
                    "model": model,
                    "effort": effort,
                    "once": once,
                });
                cli_post("/api/tasks", &body).await?;
            }
            TaskAction::Remove { id } => {
                let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
                let url = format!("http://localhost:{port}/api/tasks");
                let client = reqwest::Client::new();
                let body = serde_json::json!({ "id": id });
                let resp = client.delete(&url).json(&body).send().await?;
                println!("{}", resp.text().await?);
            }
            TaskAction::Enable { id } => {
                let body = serde_json::json!({ "id": id });
                cli_post("/api/tasks/enable", &body).await?;
            }
            TaskAction::Disable { id } => {
                let body = serde_json::json!({ "id": id });
                cli_post("/api/tasks/disable", &body).await?;
            }
            TaskAction::Runs { task } => {
                let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
                let mut url = format!("http://localhost:{port}/api/task-runs");
                if let Some(id) = &task {
                    url.push_str(&format!("?task={id}"));
                }
                let resp: serde_json::Value = reqwest::get(&url).await?.json().await?;
                let runs = resp["runs"].as_array();
                match runs {
                    Some(list) if !list.is_empty() => {
                        println!(
                            "{:<22} {:<12} {:<10} {:<10} ERROR",
                            "TIME", "TASK", "TARGET", "STATUS"
                        );
                        for r in list {
                            let ts = r["timestamp"].as_str().unwrap_or("-");
                            let name = r["task_name"].as_str().unwrap_or("-");
                            let target = r["session_name"].as_str().unwrap_or("-");
                            let status = r["status"].as_str().unwrap_or("-");
                            let err = r["error"].as_str().unwrap_or("");
                            println!(
                                "{:<22} {:<12} {:<10} {:<10} {}",
                                ts, name, target, status, err
                            );
                        }
                    }
                    _ => println!("no task runs"),
                }
            }
            TaskAction::Trigger { id } => {
                let body = serde_json::json!({ "id": id });
                cli_post("/api/tasks/trigger", &body).await?;
            }
        },
    }

    Ok(())
}

async fn setup_nostr_transport(
    state: &state::SharedState,
    ticket: Option<&str>,
    cli_relays: Vec<String>,
) {
    let transport = match nostr_transport::ensure_active(state, cli_relays).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("nostr transport setup failed: {e}");
            return;
        }
    };

    if let Some(ticket) = ticket
        && let Err(e) = transport.connect(ticket, state.clone(), true).await
    {
        tracing::warn!("failed to connect to ticket node: {e}");
    }

    reconnect_persisted_nodes(state.clone()).await;
    transport::broadcast_local_sessions(state).await;
}

fn abandoned_lease_owns_staged_row(
    session: &persistence::PersistedSession,
    lease: &crate::daemon_protocol::LifecycleLease,
) -> bool {
    if lease.owner.session_id != session.id {
        return false;
    }
    match lease.phase {
        crate::daemon_protocol::LifecyclePhase::Starting => {
            session.metadata.session_incarnation == lease.owner.incarnation
        }
        crate::daemon_protocol::LifecyclePhase::Restarting => {
            let owns_recorded_inert_pane = lease.inert_pane.is_some()
                && lease.inert_pane.as_ref() == session.pane.as_ref()
                && lease.inert_pane_owner.as_ref().is_some_and(|owner| {
                    owner.session_id == session.id
                        && owner.incarnation == session.metadata.session_incarnation
                });
            let owns_staged_target = lease.restart_target_owner.as_ref().map_or_else(
                || session.metadata.session_incarnation != lease.owner.incarnation,
                |owner| {
                    owner.session_id == session.id
                        && owner.incarnation == session.metadata.session_incarnation
                },
            );
            owns_recorded_inert_pane || owns_staged_target
        }
        crate::daemon_protocol::LifecyclePhase::Stopping => {
            session.metadata.session_incarnation == lease.owner.incarnation
                && lease.inert_pane.as_ref() == session.pane.as_ref()
                && lease.inert_pane_owner.as_ref() == Some(&lease.owner)
        }
    }
}

fn lifecycle_lease_pane_owners(
    lease: &crate::daemon_protocol::LifecycleLease,
) -> Vec<crate::daemon_protocol::ResourceOwner> {
    let mut owners = vec![lease.owner.clone()];
    if let Some(owner) = &lease.restart_target_owner
        && !owners.contains(owner)
    {
        owners.push(owner.clone());
    }
    if let Some(owner) = &lease.inert_pane_owner
        && !owners.contains(owner)
    {
        owners.push(owner.clone());
    }
    owners
}

fn persisted_session_from_entry(
    entry: &crate::daemon_protocol::SessionEntry,
) -> Option<persistence::PersistedSession> {
    if !matches!(entry.origin, crate::daemon_protocol::Origin::Local) {
        return None;
    }
    let metadata = &entry.metadata;
    let timestamp =
        chrono::DateTime::from_timestamp(entry.registered_at, 0).unwrap_or_else(chrono::Utc::now);
    Some(persistence::PersistedSession {
        id: entry.id.clone(),
        pane: entry.pane.clone(),
        registered_at: timestamp,
        last_activity_at: timestamp,
        metadata: state::SessionMetadata {
            vim_mode: metadata.vim_mode,
            project_dir: metadata.project_dir.clone(),
            canonical_project_identity: metadata.canonical_project_identity.clone(),
            role: metadata.role.clone(),
            networked: metadata.networked,
            last_metadata_update: metadata
                .last_metadata_update
                .and_then(|value| chrono::DateTime::from_timestamp(value, 0)),
            backend_session_id: metadata.backend_session_id.clone(),
            backend: metadata.backend.clone(),
            opencode_binding: metadata.opencode_binding.clone(),
            restart_generation: metadata.restart_generation,
            backend_repair_reservation: metadata.backend_repair_reservation.clone(),
            session_incarnation: metadata.session_incarnation,
            project_description: metadata.project_description.clone(),
            bulletin: metadata.bulletin.clone(),
            worktree: metadata.worktree,
            model: metadata.model.clone(),
            effort: metadata.effort.clone(),
            codex_home: metadata.codex_home.clone(),
            reminder: metadata.reminder.clone(),
            parent_session: metadata.parent_session.clone(),
            idle_policy: metadata.idle_policy.clone(),
            prompt: metadata.prompt.clone(),
            iteration: metadata.iteration,
            iteration_log: metadata.iteration_log.clone(),
            last_iteration_at: metadata.last_iteration_at,
            on_fire: metadata.on_fire.clone(),
            worktree_present: metadata.worktree_present,
            fresh_context_after_active_secs: metadata.fresh_context_after_active_secs,
            active_context_accumulated_secs: metadata.active_context_accumulated_secs,
            active_context_segment_started_at: metadata.active_context_segment_started_at,
            active_context_restart_due: metadata.active_context_restart_due,
            active_context_accounting_provisional: metadata.active_context_accounting_provisional,
        },
    })
}

async fn restore_persisted_sessions(state: &state::SharedState) -> anyhow::Result<()> {
    let persisted = persistence::load_sessions(&state.config.data_dir)
        .context("failed to restore lifecycle authority from sessions.json")?;
    let persistence::PersistedLifecycleState {
        mut sessions,
        dormant_sessions,
        incarnation_high_water,
        lifecycle_leases,
        ..
    } = persisted;
    let abandoned_leases: Vec<_> = lifecycle_leases.values().cloned().collect();

    // Restore allocator authority even when every persisted session is dead or
    // has since been removed. Reusing one of those tokens would let delayed
    // work from the prior daemon incarnation mutate a replacement.
    {
        let mut proto = state.protocol.write().await;
        proto.restore_incarnation_high_water(incarnation_high_water);
        proto.dormant_sessions = dormant_sessions.clone();
        proto.lifecycle_leases = lifecycle_leases;
    }

    // A persisted lifecycle lease proves the daemon stopped before the
    // backend-command boundary completed. Remove only its exact inert pane,
    // discard only the staged session row owned by that lease, then durably
    // release the public ID. A Restarting lease whose row still has the
    // incumbent incarnation stopped before staging and therefore preserves it.
    if !abandoned_leases.is_empty() {
        // A Restarting lease records every newly-created HTTP backend before
        // attach or prompt work. Delete that exact target-owned resource
        // before restoring the incumbent and releasing the public ID.
        for lease in &abandoned_leases {
            let (Some(backend), Some(backend_session_id), Some(backend_session_owner)) = (
                lease.backend.as_deref(),
                lease.backend_session_id.as_deref(),
                lease.backend_session_owner.as_ref(),
            ) else {
                continue;
            };
            if lease.phase != crate::daemon_protocol::LifecyclePhase::Restarting
                || !state.backends.uses_http_delivery(backend)
            {
                continue;
            }
            if lease.restart_target_owner.as_ref() != Some(backend_session_owner) {
                anyhow::bail!(
                    "abandoned restart backend owner does not match target for '{}'",
                    lease.owner.session_id
                );
            }
            let persisted_sharer = sessions.iter().any(|session| {
                session.metadata.backend.as_deref() == Some(backend)
                    && session.metadata.backend_session_id.as_deref() == Some(backend_session_id)
                    && session.metadata.session_incarnation != backend_session_owner.incarnation
            }) || dormant_sessions.values().any(|dormant| {
                dormant.metadata.backend.as_deref() == Some(backend)
                    && dormant.metadata.backend_session_id.as_deref() == Some(backend_session_id)
            });
            if persisted_sharer {
                tracing::info!(
                    backend,
                    backend_session_id,
                    "skipping abandoned restart backend cleanup: replacement session owns it"
                );
                continue;
            }
            let port = state.opencode_serve_port();
            let backend_session_id_segment = encode_path_segment(backend_session_id);
            let url = format!("http://127.0.0.1:{port}/session/{backend_session_id_segment}");
            let response = state
                .http_client
                .delete(&url)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
                .with_context(|| {
                    format!(
                        "failed to delete abandoned restart backend session {backend_session_id} for lifecycle owner {backend_session_owner:?}"
                    )
                })?;
            if !response.status().is_success()
                && response.status() != reqwest::StatusCode::NOT_FOUND
            {
                anyhow::bail!(
                    "backend cleanup for abandoned restart session '{}' returned {}",
                    backend_session_id,
                    response.status()
                );
            }
        }

        // A durable Stopping lease can outlive its registry row. Finish the
        // exact HTTP-backend abort obligation before releasing any pane,
        // worktree, row, or public-ID authority. A different persisted owner
        // of the same backend identity wins and must never be aborted.
        for lease in &abandoned_leases {
            let (Some(backend), Some(backend_session_id), Some(backend_session_owner)) = (
                lease.backend.as_deref(),
                lease.backend_session_id.as_deref(),
                lease.backend_session_owner.as_ref(),
            ) else {
                continue;
            };
            if lease.phase != crate::daemon_protocol::LifecyclePhase::Stopping
                || !state.backends.uses_http_delivery(backend)
            {
                continue;
            }
            let persisted_sharer = sessions.iter().any(|session| {
                session.metadata.backend.as_deref() == Some(backend)
                    && session.metadata.backend_session_id.as_deref() == Some(backend_session_id)
                    && !abandoned_lease_owns_staged_row(session, lease)
            }) || dormant_sessions.values().any(|dormant| {
                dormant.metadata.backend.as_deref() == Some(backend)
                    && dormant.metadata.backend_session_id.as_deref() == Some(backend_session_id)
            });
            if persisted_sharer {
                tracing::info!(
                    backend,
                    backend_session_id,
                    "skipping abandoned backend abort: persisted session still owns it"
                );
                continue;
            }
            if backend_session_owner != &lease.owner {
                anyhow::bail!(
                    "abandoned backend abort owner does not match stopping lease for '{}'",
                    lease.owner.session_id
                );
            }
            let port = state.opencode_serve_port();
            let backend_session_id_segment = encode_path_segment(backend_session_id);
            let url = format!("http://127.0.0.1:{port}/session/{backend_session_id_segment}/abort");
            let response = state
                .http_client
                .post(&url)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
                .with_context(|| {
                    format!(
                        "failed to abort abandoned backend session {backend_session_id} for lifecycle owner {backend_session_owner:?}"
                    )
                })?;
            if !response.status().is_success()
                && response.status() != reqwest::StatusCode::NOT_FOUND
            {
                anyhow::bail!(
                    "backend abort for abandoned session '{}' returned {}",
                    backend_session_id,
                    response.status()
                );
            }
        }

        let inert_panes: Vec<_> = abandoned_leases
            .iter()
            .filter_map(|lease| {
                lease
                    .inert_pane
                    .clone()
                    .map(|pane| (pane, lifecycle_lease_pane_owners(lease)))
            })
            .collect();
        if !inert_panes.is_empty() {
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                for (pane, expected_owners) in inert_panes {
                    let live_owner = crate::tmux::inspect_pane_owner(&pane)?;
                    if !live_owner
                        .as_ref()
                        .is_some_and(|owner| expected_owners.contains(owner))
                    {
                        continue;
                    }
                    let status = std::process::Command::new("tmux")
                        .args(["kill-pane", "-t", &pane])
                        .status()
                        .with_context(|| format!("failed to kill abandoned pane {pane}"))?;
                    let remaining_owner = crate::tmux::inspect_pane_owner(&pane)?;
                    if !status.success()
                        && remaining_owner
                            .as_ref()
                            .is_some_and(|owner| expected_owners.contains(owner))
                    {
                        anyhow::bail!(
                            "failed to remove abandoned pane {pane} for lifecycle owner {remaining_owner:?}"
                        );
                    }
                }
                Ok(())
            })
            .await
            .context("abandoned start pane reconciliation task failed")?
            .context("failed to reconcile abandoned start panes")?;
        }

        // Project-directory claims cover both pre-launch and terminating
        // crash envelopes. Serialize each cleanup with live claims and
        // preserve any directory shared by a persisted row that this lease
        // does not own.
        for lease in &abandoned_leases {
            let (Some(project_dir), Some(project_dir_owner)) =
                (&lease.project_dir, &lease.project_dir_owner)
            else {
                continue;
            };
            if !lease.project_dir_cleanup_on_abandon {
                continue;
            }
            let project_dir_identity = crate::state::project_dir_identity(project_dir);
            let persisted_sharer =
                sessions.iter().any(|session| {
                    session.metadata.project_dir.as_deref().is_some_and(|dir| {
                        crate::state::project_dir_identity(dir) == project_dir_identity
                    }) && !abandoned_lease_owns_staged_row(session, lease)
                }) || lease.restart_previous.as_deref().is_some_and(|previous| {
                    previous.metadata.project_dir.as_deref().is_some_and(|dir| {
                        crate::state::project_dir_identity(dir) == project_dir_identity
                    })
                }) || dormant_sessions.values().any(|dormant| {
                    dormant.metadata.project_dir.as_deref().is_some_and(|dir| {
                        crate::state::project_dir_identity(dir) == project_dir_identity
                    }) || crate::state::project_dir_identity(&dormant.canonical_project_identity)
                        == project_dir_identity
                });
            if persisted_sharer {
                tracing::info!(
                    "skipping abandoned worktree cleanup for {project_dir}: persisted session still uses it"
                );
                continue;
            }
            state
                .cleanup_worktree_dir_if_unused(project_dir_owner, project_dir)
                .await;
        }

        for lease in &abandoned_leases {
            let (
                crate::daemon_protocol::LifecyclePhase::Restarting,
                Some(target_owner),
                Some(previous),
            ) = (
                lease.phase,
                lease.restart_target_owner.as_ref(),
                lease.restart_previous.as_deref(),
            )
            else {
                continue;
            };
            let Some(position) = sessions.iter().position(|session| {
                session.id == target_owner.session_id
                    && session.metadata.session_incarnation == target_owner.incarnation
            }) else {
                continue;
            };
            let previous = persisted_session_from_entry(previous).ok_or_else(|| {
                anyhow::anyhow!(
                    "restart recovery for '{}' recorded a non-local incumbent",
                    lease.owner.session_id
                )
            })?;
            sessions[position] = previous;
        }

        sessions.retain(|session| {
            !abandoned_leases
                .iter()
                .find(|lease| lease.owner.session_id == session.id)
                .is_some_and(|lease| abandoned_lease_owns_staged_row(session, lease))
        });

        {
            let mut proto = state.protocol.write().await;
            for lease in &abandoned_leases {
                proto.abort_lifecycle(&lease.owner);
            }
        }
        persistence::save_sessions(
            &state.config.data_dir,
            &persistence::PersistedLifecycleState::new(
                sessions.clone(),
                dormant_sessions.clone(),
                incarnation_high_water,
                std::collections::BTreeMap::new(),
            ),
        )
        .context("failed to persist reconciled lifecycle authority")?;
    }

    if sessions.is_empty() {
        return Ok(());
    }

    // HTTP-delivered sessions (opencode shared serve) are reachable over their
    // API regardless of the tmux pane, so pane-process liveness must not gate
    // their restoration — same reaper-false-positive class as the live reaper.
    // Keep them unconditionally; only pane-bound (TUI) sessions need the check.
    let (http_delivered, pane_bound): (Vec<_>, Vec<_>) = sessions.into_iter().partition(|ps| {
        ps.metadata
            .backend
            .as_deref()
            .is_some_and(|b| state.backends.uses_http_delivery(b))
    });

    // Check pane liveness on blocking thread
    let names: Vec<String> = state.backends.all_process_names();
    let mut alive = tokio::task::spawn_blocking(move || {
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        pane_bound
            .into_iter()
            .filter(|ps| {
                ps.pane
                    .as_ref()
                    .is_some_and(|p| crate::tmux::pane_alive(p, &name_refs))
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    alive.extend(http_delivered);

    if alive.is_empty() {
        return Ok(());
    }

    let marker_effects = {
        let mut proto = state.protocol.write().await;
        let mut marker_effects = Vec::new();
        for ps in &alive {
            let entry = crate::daemon_protocol::SessionEntry {
                id: ps.id.clone(),
                pane: ps.pane.clone(),
                origin: crate::daemon_protocol::Origin::Local,
                metadata: metadata_for_restored_session(&ps.metadata),
                ..Default::default()
            };
            let owner = entry.owner();
            if let Some(pane) = &entry.pane {
                marker_effects.extend([
                    crate::daemon_protocol::Effect::SetTmuxVar {
                        owner: owner.clone(),
                        pane: pane.clone(),
                        name: "@ouija_session".into(),
                        value: entry.id.clone(),
                    },
                    crate::daemon_protocol::Effect::SetTmuxVar {
                        owner: owner.clone(),
                        pane: pane.clone(),
                        name: "@ouija_id".into(),
                        value: entry.id.clone(),
                    },
                    crate::daemon_protocol::Effect::SetTmuxVar {
                        owner: owner.clone(),
                        pane: pane.clone(),
                        name: "@ouija_incarnation".into(),
                        value: entry.metadata.session_incarnation.to_string(),
                    },
                ]);
            }
            if let Some(pane) = entry.session_agent_pane() {
                marker_effects.push(crate::daemon_protocol::Effect::SpawnAgent {
                    owner,
                    pane: pane.map(String::from),
                });
            }
            proto.sessions.insert(ps.id.clone(), entry);
        }
        marker_effects
    };

    // Startup rehydration bypasses Event::Register so it can preserve the
    // durable incarnation exactly. Re-emit Register's pane markers and exact
    // eligible activity receivers after the rows are visible; the normal pane
    // resource gate and physical-owner check reject a conflicting live
    // incarnation.
    let _ = state.execute_effects(&marker_effects).await;
    tracing::info!("restored {} persisted sessions", alive.len());
    Ok(())
}

fn metadata_for_restored_session(
    metadata: &state::SessionMetadata,
) -> crate::daemon_protocol::SessionMeta {
    crate::daemon_protocol::metadata_to_session_meta(Some(metadata))
}

async fn register_human_sessions(state: &state::AppState) {
    let humans = state.settings.read().await.human_sessions.clone();
    if humans.is_empty() {
        return;
    }

    let mut proto = state.protocol.write().await;
    for h in &humans {
        if proto.sessions.contains_key(&h.name) {
            tracing::debug!("human session '{}' already registered", h.name);
            continue;
        }
        let entry = crate::daemon_protocol::SessionEntry {
            id: h.name.clone(),
            pane: None,
            origin: crate::daemon_protocol::Origin::Human(h.npub.clone()),
            metadata: crate::daemon_protocol::SessionMeta {
                role: Some("human".to_string()),
                networked: false,
                ..Default::default()
            },
            ..Default::default()
        };
        proto.sessions.insert(h.name.clone(), entry);
        tracing::info!("registered human session: {}", h.name);
    }
}

async fn reconnect_persisted_nodes(state: state::SharedState) {
    let conns = match persistence::load_connections(&state.config.data_dir) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("failed to load persisted connections: {e}");
            return;
        }
    };

    let Some(transport) = state.transport_by_name("nostr").await else {
        tracing::warn!("skipping node reconnection: nostr transport not active");
        return;
    };

    let mut reconnected = 0;
    let mut connected_npubs = std::collections::HashSet::new();

    for conn in &conns {
        // Skip legacy (non-nostr) connections
        if !conn.ticket.starts_with("nprofile1") {
            tracing::info!("skipping legacy non-nostr connection");
            continue;
        }

        let label = match &conn.node_name {
            Some(name) => name.clone(),
            None => "unnamed".to_string(),
        };

        // Skip duplicate connections to the same daemon
        let npub = conn
            .daemon_npub
            .clone()
            .or_else(|| crate::api::extract_npub(&conn.ticket));
        if let Some(ref npub) = npub {
            connected_npubs.insert(npub.clone());
            let node_name = conn
                .node_name
                .as_deref()
                .unwrap_or(&npub[..16.min(npub.len())]);
            if let Err(existing) = state.try_add_node(npub, node_name) {
                tracing::info!(
                    "skipping duplicate connection to {label} (already connected as '{existing}')"
                );
                continue;
            }
        }

        tracing::info!("reconnecting to {label}...");
        match transport.connect(&conn.ticket, state.clone(), false).await {
            Ok(()) => reconnected += 1,
            Err(e) => tracing::warn!("failed to reconnect to {label}: {e}"),
        }
    }

    // Fallback: reconnect peers from peer_pubkeys.json not in connections.json.
    // This handles the case where the receiving side never persisted connection
    // info (pre-fix) or where connections.json was lost.
    let peer_pubkeys = nostr_transport::load_peer_pubkeys(&state.config.data_dir);
    let relay_urls = nostr_transport::load_relays(&state.config.data_dir);
    if !peer_pubkeys.is_empty() && !relay_urls.is_empty() {
        use nostr_sdk::prelude::*;
        let relay_parsed: Vec<RelayUrl> = relay_urls
            .iter()
            .filter_map(|u| RelayUrl::parse(u).ok())
            .collect();

        for pubkey in &peer_pubkeys {
            let npub = match pubkey.to_bech32() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if connected_npubs.contains(&npub) {
                continue;
            }

            let label = &npub[..16.min(npub.len())];
            if let Err(existing) = state.try_add_node(&npub, label) {
                tracing::info!(
                    "skipping duplicate peer_pubkey connection to {label} (already connected as '{existing}')"
                );
                continue;
            }

            let profile = Nip19Profile::new(*pubkey, relay_parsed.clone());
            let nprofile = match profile.to_bech32() {
                Ok(s) => s,
                Err(_) => continue,
            };

            tracing::info!("reconnecting to peer_pubkey {label}...");
            match transport.connect(&nprofile, state.clone(), false).await {
                Ok(()) => {
                    // Persist so future reconnects use connections.json directly
                    if let Err(e) = persistence::add_connection(
                        &state.config.data_dir,
                        &nprofile,
                        None,
                        Some(&npub),
                    ) {
                        tracing::warn!("failed to persist fallback connection: {e}");
                    }
                    reconnected += 1;
                }
                Err(e) => tracing::warn!("failed to reconnect to peer_pubkey {label}: {e}"),
            }
        }
    }

    if reconnected > 0 {
        tracing::info!("reconnected to {reconnected} persisted nodes");
    }
}

fn preflight_checks() {
    use std::process::Command as Cmd;

    if Cmd::new("tmux").arg("-V").output().is_err() {
        eprintln!("error: tmux not found");
        eprintln!();
        eprintln!("ouija requires tmux. Install it:");
        eprintln!("  apt install tmux        # Debian/Ubuntu");
        eprintln!("  brew install tmux       # macOS");
        eprintln!("  pacman -S tmux          # Arch");
        std::process::exit(1);
    }

    let backend = backend::claude_code::ClaudeCode;
    if !backend.is_available() {
        eprintln!("warning: {} not found on PATH", backend.cli_name());
        eprintln!(
            "  Sessions won't auto-register. Install: https://docs.anthropic.com/en/docs/claude-code"
        );
        eprintln!();
    }
}

const OUIJA_SYSTEMD_UNIT: &str = "ouija.service";

#[derive(Debug, PartialEq, Eq)]
struct DaemonStopPlan {
    stop_systemd: bool,
    stop_legacy: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum DaemonLifecyclePlan {
    LegacyOnly,
    SystemdOnly,
    SystemdAfterLegacyCleanup,
}

#[derive(Debug, Default)]
struct LegacyStopOutcome {
    tmux_killed: bool,
    process_killed: bool,
}

impl LegacyStopOutcome {
    fn stopped_anything(&self) -> bool {
        self.tmux_killed || self.process_killed
    }
}

#[derive(Debug, Default)]
struct DaemonStopOutcome {
    systemd_stopped: bool,
    legacy: LegacyStopOutcome,
}

impl DaemonStopOutcome {
    fn stopped_anything(&self) -> bool {
        self.systemd_stopped || self.legacy.stopped_anything()
    }
}

fn plan_daemon_stop(systemd_unit_available: bool) -> DaemonStopPlan {
    DaemonStopPlan {
        stop_systemd: systemd_unit_available,
        // Preserve stop-server's user-facing contract: stop any ouija daemon,
        // including old tmux/manual processes left behind during migration.
        stop_legacy: true,
    }
}

fn plan_supervised_lifecycle(
    systemd_unit_available: bool,
    systemd_unit_active: bool,
) -> DaemonLifecyclePlan {
    if !systemd_unit_available {
        DaemonLifecyclePlan::LegacyOnly
    } else if systemd_unit_active {
        DaemonLifecyclePlan::SystemdOnly
    } else {
        DaemonLifecyclePlan::SystemdAfterLegacyCleanup
    }
}

fn legacy_cleanup_settle_delay(plan: &DaemonLifecyclePlan) -> Option<std::time::Duration> {
    match plan {
        DaemonLifecyclePlan::SystemdAfterLegacyCleanup => Some(std::time::Duration::from_secs(1)),
        DaemonLifecyclePlan::LegacyOnly | DaemonLifecyclePlan::SystemdOnly => None,
    }
}

fn wait_for_legacy_cleanup_if_needed(plan: &DaemonLifecyclePlan) {
    if let Some(delay) = legacy_cleanup_settle_delay(plan) {
        std::thread::sleep(delay);
    }
}

fn systemd_user_unit_available() -> bool {
    use std::process::Command as Cmd;

    Cmd::new("systemctl")
        .args(["--user", "cat", OUIJA_SYSTEMD_UNIT])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn systemd_user_unit_active() -> bool {
    use std::process::Command as Cmd;

    Cmd::new("systemctl")
        .args(["--user", "is-active", "--quiet", OUIJA_SYSTEMD_UNIT])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn systemctl_user(action: &str) -> anyhow::Result<()> {
    use std::process::Command as Cmd;

    let status = Cmd::new("systemctl")
        .args(["--user", action, OUIJA_SYSTEMD_UNIT])
        .status()
        .with_context(|| format!("failed to run systemctl --user {action} {OUIJA_SYSTEMD_UNIT}"))?;
    if !status.success() {
        anyhow::bail!("systemctl --user {action} {OUIJA_SYSTEMD_UNIT} failed");
    }
    Ok(())
}

fn stop_legacy_daemon() -> LegacyStopOutcome {
    use std::process::Command as Cmd;

    // Kill the ouija-daemon tmux session if it exists
    let tmux_killed = Cmd::new("tmux")
        .args(["kill-session", "-t", "ouija-daemon"])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    // Also kill any "ouija start-server" processes
    let pkill_killed = Cmd::new("pkill")
        .args(["-f", "ouija start-server"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    LegacyStopOutcome {
        tmux_killed,
        process_killed: pkill_killed,
    }
}

fn stop_daemon_processes_with(
    systemd_available: bool,
    systemd_active: bool,
    mut stop_systemd: impl FnMut() -> anyhow::Result<()>,
    mut stop_legacy: impl FnMut() -> LegacyStopOutcome,
) -> anyhow::Result<DaemonStopOutcome> {
    let plan = plan_daemon_stop(systemd_available);

    let mut systemd_stopped = false;
    let mut systemd_error = None;
    if plan.stop_systemd {
        match stop_systemd() {
            Ok(()) => systemd_stopped = systemd_active,
            Err(err) => systemd_error = Some(err),
        }
    }

    let legacy = if plan.stop_legacy {
        stop_legacy()
    } else {
        LegacyStopOutcome::default()
    };

    if let Some(err) = systemd_error {
        return Err(err);
    }

    Ok(DaemonStopOutcome {
        systemd_stopped,
        legacy,
    })
}

fn stop_daemon_processes() -> anyhow::Result<DaemonStopOutcome> {
    let systemd_available = systemd_user_unit_available();
    let systemd_active = systemd_available && systemd_user_unit_active();
    stop_daemon_processes_with(
        systemd_available,
        systemd_active,
        || systemctl_user("stop"),
        stop_legacy_daemon,
    )
}

fn spawn_legacy_daemon() -> anyhow::Result<()> {
    use std::process::Command as Cmd;

    Cmd::new("ouija")
        .arg("start-server")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn ouija start-server")?;
    Ok(())
}

fn start_daemon() -> anyhow::Result<()> {
    let systemd_available = systemd_user_unit_available();
    let systemd_active = systemd_available && systemd_user_unit_active();
    let plan = plan_supervised_lifecycle(systemd_available, systemd_active);
    match plan {
        DaemonLifecyclePlan::LegacyOnly => spawn_legacy_daemon(),
        DaemonLifecyclePlan::SystemdOnly => systemctl_user("start"),
        DaemonLifecyclePlan::SystemdAfterLegacyCleanup => {
            let _ = stop_legacy_daemon();
            wait_for_legacy_cleanup_if_needed(&plan);
            systemctl_user("start")
        }
    }
}

fn restart_daemon() -> anyhow::Result<()> {
    let systemd_available = systemd_user_unit_available();
    let systemd_active = systemd_available && systemd_user_unit_active();
    let plan = plan_supervised_lifecycle(systemd_available, systemd_active);
    match plan {
        DaemonLifecyclePlan::LegacyOnly => {
            let _ = stop_legacy_daemon();
            std::thread::sleep(std::time::Duration::from_secs(1));
            spawn_legacy_daemon()
        }
        DaemonLifecyclePlan::SystemdOnly => systemctl_user("restart"),
        DaemonLifecyclePlan::SystemdAfterLegacyCleanup => {
            let _ = stop_legacy_daemon();
            wait_for_legacy_cleanup_if_needed(&plan);
            systemctl_user("restart")
        }
    }
}

fn daemon_http_alive(status_url: &str) -> bool {
    use std::process::Command as Cmd;

    Cmd::new("curl")
        .args(["-sf", status_url])
        .stdout(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn wait_for_daemon(status_url: &str) -> bool {
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if daemon_http_alive(status_url) {
            return true;
        }
    }
    false
}

fn sync_current_exe_from_cargo_bin() {
    // Replace the running binary with the new one. We can't fs::copy over a
    // running executable (ETXTBSY), but we can unlink it first — the kernel
    // keeps the old inode alive for this process while the path becomes free.
    let cargo_bin = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cargo/bin/ouija"))
        .unwrap_or_default();
    let current_exe = std::env::current_exe().unwrap_or_default();
    if cargo_bin.exists() && current_exe != cargo_bin && current_exe.exists() {
        let _ = std::fs::remove_file(&current_exe);
        if let Err(e) = std::fs::copy(&cargo_bin, &current_exe) {
            eprintln!("warning: could not update {}: {e}", current_exe.display());
        }
    }
}

fn stop_daemon() -> anyhow::Result<()> {
    let outcome = stop_daemon_processes()?;
    if outcome.stopped_anything() {
        println!("ouija daemon stopped");
    } else {
        println!("no running daemon found");
    }
    Ok(())
}

fn update_and_restart() -> anyhow::Result<()> {
    use std::process::Command as Cmd;

    let latest = fetch_latest_crate_version("ouija")?;
    let current = env!("CARGO_PKG_VERSION");
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let status_url = format!("http://localhost:{port}/api/status");
    let daemon_alive = daemon_http_alive(&status_url);

    if latest == current {
        println!("already on latest version ({current})");
        backend::claude_code::refresh_plugin_cache(&latest);
        if !daemon_alive {
            println!("daemon is not running — starting it...");
            start_daemon()?;
            if !wait_for_daemon(&status_url) {
                eprintln!("warning: daemon did not start within 10s");
            }
        }
        println!("dashboard: http://localhost:{port}");
        return Ok(());
    }
    println!("updating ouija {current} -> {latest}...");

    let status = Cmd::new("cargo")
        .args(["install", "ouija", "--version", &latest])
        .status()
        .context("failed to run cargo install")?;
    if !status.success() {
        anyhow::bail!("cargo install ouija --version {latest} failed");
    }

    backend::claude_code::refresh_plugin_cache(&latest);

    // Check if opencode serve is running — it needs a restart to pick up plugin changes
    let serve_running = Cmd::new("pgrep")
        .args(["-f", "opencode serve"])
        .stdout(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if serve_running {
        println!(
            "note: opencode serve is running — restart it to pick up plugin changes:\n  \
             pkill -f 'opencode serve' && opencode serve --port 8200 --hostname 127.0.0.1 &"
        );
    }

    sync_current_exe_from_cargo_bin();

    println!("restarting daemon...");
    restart_daemon()?;

    if wait_for_daemon(&status_url) {
        println!("ouija updated to {latest} and running");
        println!("dashboard: http://localhost:{port}");
        return Ok(());
    }
    anyhow::bail!("daemon did not start within 10s")
}

/// Query crates.io for the latest version of a crate (including prereleases).
fn fetch_latest_crate_version(name: &str) -> anyhow::Result<String> {
    use std::process::Command as Cmd;

    // crates.io rejects requests without a descriptive User-Agent (HTTP 403),
    // so identify ourselves per their crawler policy.
    let output = Cmd::new("curl")
        .args([
            "-sf",
            "-A",
            "ouija-self-update (+https://github.com/dcadenas/ouija)",
            &format!("https://crates.io/api/v1/crates/{name}"),
        ])
        .output()
        .context("failed to query crates.io")?;
    if !output.status.success() {
        anyhow::bail!("crates.io query failed");
    }
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("invalid JSON from crates.io")?;
    // versions are sorted newest-first; pick the first non-yanked
    json["versions"]
        .as_array()
        .and_then(|versions| {
            versions
                .iter()
                .find(|v| !v["yanked"].as_bool().unwrap_or(true))
                .and_then(|v| v["num"].as_str())
                .map(String::from)
        })
        .ok_or_else(|| anyhow::anyhow!("no versions found for {name} on crates.io"))
}

/// Outcome of priority-resolving the three signals that can identify the
/// caller's session: the `@ouija_session` tmux pane var, the
/// `$OUIJA_SESSION_ID` env var, and `$TMUX_PANE`.
///
/// `LookupByPane` defers an HTTP call to the daemon; the pure decision lives
/// in [`pick_session_id`] so the precedence is testable without env-var or
/// tmux mutation.
#[derive(Debug, PartialEq, Eq)]
enum SessionIdResolution {
    Found(String, IdentitySource),
    LookupByPane(String),
    None,
}

/// Which signal produced a resolved session id. Reported by `ouija whoami`
/// so agents can see whether their identity came from a daemon-controlled
/// source or a possibly-stale environment variable.
#[derive(Debug, PartialEq, Eq)]
enum IdentitySource {
    PaneVar,
    EnvVar,
    PaneLookup,
    BackendIdentity,
}

/// The backend adapter and a local pane/environment signal identified two
/// different sessions. Neither can safely win: accepting the local value would
/// let a stale shell override a credentialed backend binding, while accepting
/// the backend value without reporting the discrepancy would hide an unsafe
/// execution context.
#[derive(Debug, PartialEq, Eq)]
struct IdentityConflict {
    local_id: String,
    local_source: IdentitySource,
    canonical_id: String,
}

/// A backend identity lookup failed before producing canonical ownership.
///
/// `outcome` is present for a structured daemon rejection and absent for
/// transport or protocol failures. Only `incomplete_legacy` can yield to an
/// independently resolved local identity: it describes non-canonical partial
/// rows, not positive evidence that the local identity belongs elsewhere.
#[derive(Debug, PartialEq, Eq)]
struct BackendIdentityLookupError {
    outcome: Option<String>,
    detail: String,
}

impl BackendIdentityLookupError {
    fn daemon_rejection(outcome: &str, detail: &str) -> Self {
        Self {
            outcome: Some(outcome.into()),
            detail: detail.into(),
        }
    }

    fn protocol_failure(detail: impl Into<String>) -> Self {
        Self {
            outcome: None,
            detail: detail.into(),
        }
    }

    fn allows_local_fallback(&self) -> bool {
        self.outcome.as_deref() == Some("incomplete_legacy")
    }
}

impl std::fmt::Display for BackendIdentityLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.outcome.as_deref() {
            Some(outcome) => write!(
                f,
                "backend identity resolution failed ({outcome}) : {}",
                self.detail
            ),
            None => f.write_str(&self.detail),
        }
    }
}

impl std::error::Error for BackendIdentityLookupError {}

/// Give a resolved backend identity precedence over local hints, but only when
/// those hints agree. This deliberately has no I/O so every caller can apply
/// the same fail-closed rule and the conflict contract remains directly
/// testable.
fn arbitrate_backend_identity(
    local: Option<(String, IdentitySource)>,
    backend_canonical: Option<String>,
) -> Result<Option<(String, IdentitySource)>, IdentityConflict> {
    let Some(canonical_id) = backend_canonical else {
        return Ok(local);
    };
    match local {
        Some((local_id, local_source)) if local_id != canonical_id => Err(IdentityConflict {
            local_id,
            local_source,
            canonical_id,
        }),
        Some(_) | None => Ok(Some((canonical_id, IdentitySource::BackendIdentity))),
    }
}

/// Select canonical backend evidence without letting partial rows strand a
/// separately resolved local identity.
fn backend_canonical_for_arbitration(
    local: Option<&(String, IdentitySource)>,
    backend_lookup: Result<String, BackendIdentityLookupError>,
) -> Result<Option<String>, BackendIdentityLookupError> {
    match backend_lookup {
        Ok(id) => Ok(Some(id)),
        Err(error) if local.is_some() && error.allows_local_fallback() => Ok(None),
        Err(error) => Err(error),
    }
}

impl std::fmt::Display for IdentitySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PaneVar => write!(f, "the @ouija_session tmux pane var"),
            Self::EnvVar => write!(f, "$OUIJA_SESSION_ID"),
            Self::PaneLookup => write!(f, "daemon lookup by $TMUX_PANE"),
            Self::BackendIdentity => write!(f, "daemon resolution of this backend identity"),
        }
    }
}

/// Anti-guessing guidance shared by every identity-failure message. The
/// misattribution incident (#1395) started with an agent inferring `--from`
/// from the project basename, which named a real sibling session.
const NO_GUESS_GUIDANCE: &str = "Never guess a sender id — not from the project directory name, \
a branch name, or an `ouija ls` entry. A guessed sender impersonates another session and \
misroutes its replies. Use only an exact id: the one in your injected system prompt \
(\"You are session \\\"<id>\\\" on the ouija mesh\") or a $OUIJA_SESSION_ID provided by your operator.";

/// Diagnostic snapshot of every identity signal `ouija whoami` inspected
/// before concluding the caller cannot be identified.
#[derive(Debug)]
struct WhoamiFailure {
    tmux_pane: Option<String>,
    pane_var: Option<String>,
    env_var: Option<String>,
    /// `Some` only when a daemon lookup by pane was attempted.
    lookup: Option<PaneLookupFailure>,
}

#[derive(Debug)]
enum PaneLookupFailure {
    DaemonUnreachable(String),
    NoSessionForPane,
}

/// Render a loud, guess-free explanation of why identity resolution failed.
fn format_whoami_failure(failure: &WhoamiFailure) -> String {
    let mut lines = vec![
        "unable to resolve this session's Ouija identity.".to_string(),
        String::new(),
        "Signals checked:".to_string(),
    ];
    match &failure.tmux_pane {
        Some(pane) => {
            lines.push(format!("  - $TMUX_PANE: {pane}"));
            match failure.pane_var.as_deref() {
                Some("") => lines.push("  - @ouija_session pane var: set but empty".to_string()),
                Some(var) => lines.push(format!("  - @ouija_session pane var: {var}")),
                None => lines.push("  - @ouija_session pane var: not set".to_string()),
            }
        }
        None => lines.push(
            "  - $TMUX_PANE: not set (this shell is not attached to a tmux pane)".to_string(),
        ),
    }
    match failure.env_var.as_deref() {
        Some("") => lines.push("  - $OUIJA_SESSION_ID: set but empty".to_string()),
        Some(var) => lines.push(format!("  - $OUIJA_SESSION_ID: {var}")),
        None => lines.push("  - $OUIJA_SESSION_ID: not set".to_string()),
    }
    match &failure.lookup {
        Some(PaneLookupFailure::DaemonUnreachable(url)) => {
            lines.push(format!("  - daemon lookup: daemon unreachable at {url}"));
        }
        Some(PaneLookupFailure::NoSessionForPane) => {
            lines.push(format!(
                "  - daemon lookup: no registered session for pane {}",
                failure.tmux_pane.as_deref().unwrap_or("?")
            ));
        }
        None => {}
    }
    lines.push(String::new());
    lines.push(NO_GUESS_GUIDANCE.to_string());
    lines.join("\n")
}

/// Message for an id that resolved from a signal but is not registered with
/// the daemon — a stale `$OUIJA_SESSION_ID` after a rename, typically.
fn format_unregistered_identity(id: &str, source: &IdentitySource) -> String {
    format!(
        "resolved id '{id}' via {source}, but no local session with that id is registered. \
         The session may have been renamed or removed, or the signal is stale. \
         Ask the operator for the correct id. {NO_GUESS_GUIDANCE}"
    )
}

/// Explain why a backend binding and a local identity signal cannot safely be
/// reconciled. This is deliberately a hard error rather than a warning: the
/// caller may be running in a stale pane or inherited shell.
fn format_identity_conflict(conflict: &IdentityConflict) -> String {
    format!(
        "backend identity resolves to canonical session '{}', but {} resolves to '{}'. \
         Refusing to send with conflicting identity signals. Restart the stale shell or ask \
         the operator to repair the session binding. {NO_GUESS_GUIDANCE}",
        conflict.canonical_id, conflict.local_source, conflict.local_id
    )
}

/// True when `/api/status` lists a *local* session with this id. Remote
/// sessions (node-prefixed) are never the local caller's own identity.
fn status_lists_local_session(status: &serde_json::Value, id: &str) -> bool {
    status["sessions"].as_array().is_some_and(|sessions| {
        sessions
            .iter()
            .any(|s| s["id"].as_str() == Some(id) && s["origin"].as_str() == Some("local"))
    })
}

/// Error text for send-path commands that cannot identify the caller.
///
/// Intentionally never instructs the caller to run `ouija register`: in
/// non-tmux engines (e.g. opencode HTTP API) an LLM reading the error
/// literally would self-trigger a ghost-shape register call. Equally, it
/// must never invite the caller to pick a plausible-looking `--from` —
/// that guess is how sender misattribution (#1395) happened.
fn unresolved_sender_error() -> String {
    format!(
        "unable to resolve the current session ID. Run `ouija whoami` for diagnostics. \
         If you already know your exact session id (from your injected system prompt or \
         $OUIJA_SESSION_ID), pass `--from <id>`. {NO_GUESS_GUIDANCE}"
    )
}

fn resolve_message(
    positional: Option<String>,
    read_stdin: bool,
    message_file: Option<PathBuf>,
) -> anyhow::Result<String> {
    let source_count = usize::from(positional.is_some())
        + usize::from(read_stdin)
        + usize::from(message_file.is_some());
    match source_count {
        0 => bail!("provide a message argument, --stdin, or --message-file <path>"),
        1 => {}
        _ => bail!("provide only one message source: argument, --stdin, or --message-file"),
    }

    if let Some(message) = positional {
        return Ok(message);
    }
    if let Some(path) = message_file {
        return std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read message file {}", path.display()));
    }

    let mut message = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut message)
        .context("failed to read message from stdin")?;
    Ok(message)
}

/// Pick the caller's session id from the three available signals.
///
/// Priority when in tmux (`tmux_pane` is `Some`):
///   1. `@ouija_session` pane var — daemon-controlled, cleared on Remove and
///      rewritten on Rename, so it always reflects current state.
///   2. `$OUIJA_SESSION_ID` env var — fallback for the race window before the
///      daemon's `SetTmuxVar` effect lands, and for opencode bash subshells
///      that occasionally lose `TMUX_PANE` inheritance.
///   3. `LookupByPane` — last-resort daemon query.
///
/// Outside tmux, only the env var can identify the caller.
///
/// The pane var must outrank the env var because `pane_env_args` exports
/// `OUIJA_SESSION_ID` once at pane fork time and tmux cannot mutate a running
/// shell's environment afterward — so a shell that outlives its originating
/// session keeps a stale env var indefinitely (issue #42).
fn pick_session_id(
    tmux_pane: Option<&str>,
    pane_var: Option<String>,
    env_var: Option<String>,
) -> SessionIdResolution {
    if tmux_pane.is_some() {
        if let Some(id) = pane_var.filter(|s| !s.is_empty()) {
            return SessionIdResolution::Found(id, IdentitySource::PaneVar);
        }
    }
    if let Some(id) = env_var.filter(|s| !s.is_empty()) {
        return SessionIdResolution::Found(id, IdentitySource::EnvVar);
    }
    if let Some(pane) = tmux_pane {
        return SessionIdResolution::LookupByPane(pane.to_string());
    }
    SessionIdResolution::None
}

/// Caller execution context sent with every `/api/send` so the daemon can
/// cross-check the claimed sender (task #1395). The `self_id` is the exact
/// result that identity arbitration selected for `from`; it must never be
/// recalculated from raw pane or environment signals after a backend identity
/// has supplied the canonical id.
fn sender_context(
    self_id: Option<&str>,
    tmux_pane: Option<String>,
    backend_identity: Option<backend::BackendSessionIdentity>,
    trusted_local_claim: bool,
) -> serde_json::Value {
    serde_json::json!({
        "pane": tmux_pane,
        "self_id": self_id,
        "backend_identity": backend_identity,
        "trusted_local_claim": trusted_local_claim,
    })
}

/// Result of running the full identity-resolution path, with enough detail
/// for `ouija whoami` to explain a failure.
enum WhoamiOutcome {
    Resolved {
        id: String,
        source: IdentitySource,
        tmux_pane: Option<String>,
        backend_identity: Option<backend::BackendSessionIdentity>,
    },
    Unresolved(WhoamiFailure),
    Conflict(IdentityConflict),
    /// An adapter identified this caller but the daemon could not prove one
    /// canonical public session. This is terminal, including transport errors:
    /// falling back to a pane/env hint would reintroduce misattribution.
    BackendResolutionFailed(String),
}

/// Run the full identity resolution path with diagnostics.
///
/// This is the single identity path: `require_my_session_id` (used by
/// ask/tell/reply/announce/rename) and `ouija whoami` both resolve through
/// here and then both run [`verify_resolved_id_registered`], so whoami's
/// answer is by construction the sender those commands would use — including
/// the registration check, which now rejects a stale id on the send path too.
/// See [`pick_session_id`] for the precedence and rationale.
async fn whoami_outcome() -> WhoamiOutcome {
    let tmux_pane = std::env::var("TMUX_PANE").ok();
    let pane_var = tmux_pane.as_deref().and_then(tmux_var::get);
    let env_var = std::env::var("OUIJA_SESSION_ID").ok();
    let backend_identity = backend::BackendRegistry::default_registry().caller_session_identity();

    let (local, lookup) =
        match pick_session_id(tmux_pane.as_deref(), pane_var.clone(), env_var.clone()) {
            SessionIdResolution::Found(id, source) => (Some((id, source)), None),
            SessionIdResolution::LookupByPane(pane) => {
                let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
                let base = format!("http://localhost:{port}");
                let status = match reqwest::get(format!("{base}/api/status")).await {
                    Ok(resp) => resp.json::<serde_json::Value>().await.ok(),
                    Err(_) => None,
                };
                match status {
                    Some(status) => {
                        let id = status["sessions"].as_array().and_then(|sessions| {
                            sessions
                                .iter()
                                .find(|s| s["pane"].as_str() == Some(&pane))
                                .and_then(|s| s["id"].as_str().map(String::from))
                        });
                        (
                            id.map(|id| (id, IdentitySource::PaneLookup)),
                            Some(PaneLookupFailure::NoSessionForPane),
                        )
                    }
                    None => (None, Some(PaneLookupFailure::DaemonUnreachable(base))),
                }
            }
            SessionIdResolution::None => (None, None),
        };

    // Resolve a native backend identity even when a local signal was found.
    // A successful binding is canonical and must therefore arbitrate (or
    // reject) the local hint rather than merely act as a fallback. An
    // incomplete legacy outcome has no canonical owner and may yield to an
    // independently resolved local identity; all other lookup failures remain
    // terminal.
    let backend_canonical = match backend_identity.as_ref() {
        Some(identity) => match backend_canonical_for_arbitration(
            local.as_ref(),
            resolve_backend_identity_from_daemon(identity).await,
        ) {
            Ok(id) => id,
            Err(error) => return WhoamiOutcome::BackendResolutionFailed(error.to_string()),
        },
        None => None,
    };
    match arbitrate_backend_identity(local, backend_canonical) {
        Ok(Some((id, source))) => WhoamiOutcome::Resolved {
            id,
            source,
            tmux_pane,
            backend_identity,
        },
        Ok(None) => WhoamiOutcome::Unresolved(WhoamiFailure {
            tmux_pane,
            pane_var,
            env_var,
            lookup,
        }),
        Err(conflict) => WhoamiOutcome::Conflict(conflict),
    }
}

/// Resolve an adapter-owned opaque identity to the daemon's canonical Local
/// public id. The raw backend ID never reaches a send envelope as `from`.
async fn resolve_backend_identity_from_daemon(
    identity: &backend::BackendSessionIdentity,
) -> Result<String, BackendIdentityLookupError> {
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!("http://localhost:{port}/api/backend-identities/resolve");
    let response = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({
            "backend": identity.backend,
            "session_id": identity.session_id,
        }))
        .send()
        .await
        .map_err(|error| {
            BackendIdentityLookupError::protocol_failure(format!(
                "could not resolve backend identity via {url}: {error}"
            ))
        })?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.map_err(|error| {
        BackendIdentityLookupError::protocol_failure(format!(
            "daemon returned invalid backend identity response: {error}"
        ))
    })?;
    if !status.is_success() {
        return Err(BackendIdentityLookupError::daemon_rejection(
            body["outcome"].as_str().unwrap_or("unknown"),
            body["error"].as_str().unwrap_or("no daemon detail"),
        ));
    }
    body["session_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(String::from)
        .ok_or_else(|| {
            BackendIdentityLookupError::protocol_failure(
                "daemon resolved backend identity without a session_id",
            )
        })
}

struct ResolvedSender {
    id: String,
    context: serde_json::Value,
}

/// Build a trusted explicit Local claim from raw caller observations.
///
/// The public id is authoritative, while pane/environment/backend signals are
/// preserved independently so the daemon can reject only positive evidence
/// that resolves to a sibling Local session. A pane lookup remains represented
/// by `pane`; the daemon already owns the authoritative pane mapping.
fn resolve_explicit_sender(
    explicit: String,
    tmux_pane: Option<String>,
    pane_var: Option<String>,
    env_var: Option<String>,
    backend_identity: Option<backend::BackendSessionIdentity>,
) -> ResolvedSender {
    let self_id = match pick_session_id(tmux_pane.as_deref(), pane_var, env_var) {
        SessionIdResolution::Found(id, _) => Some(id),
        SessionIdResolution::LookupByPane(_) | SessionIdResolution::None => None,
    };
    let context = sender_context(self_id.as_deref(), tmux_pane, backend_identity, true);
    ResolvedSender {
        id: explicit,
        context,
    }
}

fn backend_recovery_caller_evidence(
    tmux_pane: Option<String>,
    pane_var: Option<String>,
    env_var: Option<String>,
) -> crate::state::BackendRecoveryCallerEvidence {
    crate::state::BackendRecoveryCallerEvidence {
        pane: tmux_pane,
        pane_var_id: pane_var.filter(|id| !id.is_empty()),
        env_id: env_var.filter(|id| !id.is_empty()),
    }
}

/// Resolve a message sender and the observations sent alongside it.
///
/// An explicit public Local id is handled separately from fail-closed implicit
/// identity inference. The daemon validates explicit claims against its
/// authoritative session state before applying `Event::Send`.
async fn resolve_sender(explicit: Option<String>) -> anyhow::Result<ResolvedSender> {
    if let Some(explicit) = explicit {
        let tmux_pane = std::env::var("TMUX_PANE")
            .ok()
            .filter(|pane| !pane.is_empty());
        let pane_var = tmux_pane.as_deref().and_then(tmux_var::get);
        let env_var = std::env::var("OUIJA_SESSION_ID")
            .ok()
            .filter(|id| !id.is_empty());
        let backend_identity =
            backend::BackendRegistry::default_registry().caller_session_identity();
        return Ok(resolve_explicit_sender(
            explicit,
            tmux_pane,
            pane_var,
            env_var,
            backend_identity,
        ));
    }

    match whoami_outcome().await {
        WhoamiOutcome::Resolved {
            id,
            source,
            tmux_pane,
            backend_identity,
        } => {
            verify_resolved_id_registered(&id, &source).await?;
            let context = sender_context(Some(&id), tmux_pane, backend_identity, false);
            Ok(ResolvedSender { id, context })
        }
        WhoamiOutcome::Conflict(conflict) => anyhow::bail!(format_identity_conflict(&conflict)),
        WhoamiOutcome::BackendResolutionFailed(error) => anyhow::bail!(
            "backend identity was discovered but could not be resolved safely: {error}"
        ),
        WhoamiOutcome::Unresolved(_) => Err(anyhow::anyhow!(unresolved_sender_error())),
    }
}

/// Verify a resolved id is a registered *local* session, leniently.
///
/// Both `ouija whoami` and the send path ([`require_my_session_id`]) run this,
/// so a stale or renamed id (e.g. a persistent shell's `$OUIJA_SESSION_ID`
/// after a rename) fails on the send path as loudly as in `ouija whoami`
/// instead of silently stamping a wrong sender.
///
/// Only a positive disproof fails: when the daemon is unreachable or its
/// status is unparseable, we warn and accept, because an outage must not block
/// an otherwise-correct send. `PaneLookup` ids came from `/api/status` itself,
/// so registration is already proven and the round trip is skipped.
async fn verify_resolved_id_registered(id: &str, source: &IdentitySource) -> anyhow::Result<()> {
    if matches!(source, IdentitySource::PaneLookup) {
        return Ok(());
    }
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!("http://localhost:{port}/api/status");
    match reqwest::get(&url).await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(status) => {
                if status_lists_local_session(&status, id) {
                    Ok(())
                } else {
                    anyhow::bail!(format_unregistered_identity(id, source))
                }
            }
            Err(_) => {
                eprintln!(
                    "warning: could not parse daemon status, could not verify '{id}' is registered"
                );
                Ok(())
            }
        },
        Err(_) => {
            eprintln!("warning: daemon unreachable, could not verify '{id}' is registered");
            Ok(())
        }
    }
}

/// Resolve session ID or bail with a helpful error.
///
/// Resolves through [`whoami_outcome`] and then [`verify_resolved_id_registered`],
/// the exact same two steps `ouija whoami` performs — so whoami's answer is by
/// construction the sender this returns. See [`unresolved_sender_error`] for
/// why the unresolved message must not mention `ouija register` or invite a
/// guessed `--from`.
async fn require_my_session_id() -> anyhow::Result<String> {
    match whoami_outcome().await {
        WhoamiOutcome::Resolved { id, source, .. } => {
            verify_resolved_id_registered(&id, &source).await?;
            Ok(id)
        }
        WhoamiOutcome::Unresolved(_) => Err(anyhow::anyhow!(unresolved_sender_error())),
        WhoamiOutcome::Conflict(conflict) => anyhow::bail!(format_identity_conflict(&conflict)),
        WhoamiOutcome::BackendResolutionFailed(error) => anyhow::bail!(
            "backend identity was discovered but could not be resolved safely: {error}"
        ),
    }
}

/// `ouija whoami`: print the resolved session id to stdout (source note on
/// stderr, so `--from $(ouija whoami)` stays clean), or fail loudly with
/// signal-by-signal diagnostics.
///
/// Registration is verified via [`verify_resolved_id_registered`], the same
/// check the send path runs — a stale `$OUIJA_SESSION_ID` left over from a
/// rename fails here (and there) rather than stamp a wrong sender later.
async fn cli_whoami() -> anyhow::Result<()> {
    match whoami_outcome().await {
        WhoamiOutcome::Resolved { id, source, .. } => {
            verify_resolved_id_registered(&id, &source).await?;
            eprintln!("resolved via {source}");
            println!("{id}");
            Ok(())
        }
        WhoamiOutcome::Unresolved(failure) => anyhow::bail!(format_whoami_failure(&failure)),
        WhoamiOutcome::Conflict(conflict) => anyhow::bail!(format_identity_conflict(&conflict)),
        WhoamiOutcome::BackendResolutionFailed(error) => anyhow::bail!(
            "backend identity was discovered but could not be resolved safely: {error}"
        ),
    }
}

async fn cli_get(path: &str) -> anyhow::Result<()> {
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!("http://localhost:{port}{path}");
    let resp = reqwest::get(&url).await?;
    let text = resp.text().await?;
    println!("{text}");
    Ok(())
}

async fn cli_list_sessions() -> anyhow::Result<()> {
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!("http://localhost:{port}/api/status");
    let status: serde_json::Value = reqwest::get(&url).await?.json().await?;
    println!("{}", project_session_list(&status));
    Ok(())
}

async fn rollover_live_caller() -> anyhow::Result<rollover::LiveCaller> {
    let (id, backend_owned) = match whoami_outcome().await {
        WhoamiOutcome::Resolved {
            id,
            source,
            backend_identity,
            ..
        } => {
            verify_resolved_id_registered(&id, &source).await?;
            (
                id,
                source == IdentitySource::BackendIdentity && backend_identity.is_some(),
            )
        }
        WhoamiOutcome::Unresolved(_) => anyhow::bail!(unresolved_sender_error()),
        WhoamiOutcome::Conflict(conflict) => anyhow::bail!(format_identity_conflict(&conflict)),
        WhoamiOutcome::BackendResolutionFailed(error) => anyhow::bail!(
            "backend identity was discovered but could not be resolved safely: {error}"
        ),
    };
    let before = fetch_rollover_incarnation(&id).await?;
    let incarnation =
        verify_rollover_incarnation_evidence(local_incarnation_hint()?, backend_owned, before)?;
    let cwd = std::env::current_dir().context("reading current directory for rollover")?;
    let caller = rollover::capture_live_caller(id.clone(), incarnation, &cwd)?;
    let after = fetch_rollover_incarnation(&id).await?;
    if after != before {
        anyhow::bail!(
            "session incarnation changed while capturing rollover state ({before} -> {after})"
        );
    }
    Ok(caller)
}

fn verify_rollover_incarnation_evidence(
    local_hint: Option<u64>,
    backend_owned: bool,
    daemon_incarnation: u64,
) -> anyhow::Result<u64> {
    match local_hint {
        Some(local) if local == daemon_incarnation => Ok(local),
        Some(local) => anyhow::bail!(
            "local session incarnation {local} does not match daemon incarnation {daemon_incarnation}"
        ),
        None if backend_owned => Ok(daemon_incarnation),
        None => anyhow::bail!(
            "rollover requires exact caller-owned incarnation evidence from the pane marker, \
             OUIJA_SESSION_INCARNATION, or a bound backend identity"
        ),
    }
}

async fn fetch_rollover_incarnation(id: &str) -> anyhow::Result<u64> {
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!(
        "http://localhost:{port}/api/sessions/{}",
        encode_path_segment(id)
    );
    let response = reqwest::get(&url).await?;
    let status = response.status();
    let text = response.text().await?;
    let body = classify_http_response(status, &text)?;
    let session: serde_json::Value =
        serde_json::from_str(&body).context("parsing live Ouija session metadata")?;
    rollover_incarnation_from_session(&session, id)
}

fn rollover_incarnation_from_session(
    session: &serde_json::Value,
    expected_id: &str,
) -> anyhow::Result<u64> {
    if session["id"].as_str() != Some(expected_id) {
        anyhow::bail!("daemon returned a different session while preparing rollover");
    }
    if session["origin"].as_str() != Some("local") {
        anyhow::bail!("rollover is available only to local Ouija sessions");
    }
    session["session_incarnation"]
        .as_str()
        .context("daemon status does not expose session_incarnation")
        .and_then(|value| {
            value
                .parse()
                .context("daemon returned a non-decimal session_incarnation")
        })
}

fn local_incarnation_hint() -> anyhow::Result<Option<u64>> {
    let marker = std::env::var("TMUX_PANE").ok().and_then(|pane| {
        std::process::Command::new("tmux")
            .args([
                "display",
                "-p",
                "-t",
                pane.as_str(),
                "#{@ouija_incarnation}",
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    });
    let hint = marker.or_else(|| std::env::var("OUIJA_SESSION_INCARNATION").ok());
    hint.map(|value| {
        value
            .parse()
            .with_context(|| format!("invalid local session incarnation {value:?}"))
    })
    .transpose()
}

fn project_session_list(status: &serde_json::Value) -> serde_json::Value {
    let sessions = status
        .get("sessions")
        .and_then(|sessions| sessions.as_array())
        .map(|sessions| {
            sessions
                .iter()
                .map(|session| {
                    let mut projected = serde_json::Map::new();
                    projected.insert(
                        "id".to_string(),
                        session
                            .get("id")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    );
                    projected.insert(
                        "origin".to_string(),
                        session
                            .get("origin")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    );
                    if let Some(incarnation) = session
                        .get("session_incarnation")
                        .and_then(|value| value.as_str())
                    {
                        projected.insert(
                            "session_incarnation".to_string(),
                            serde_json::Value::String(incarnation.to_string()),
                        );
                    }
                    projected.insert(
                        "parent_session".to_string(),
                        session
                            .get("parent_session")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    );

                    if let Some(project) = session
                        .get("project_dir")
                        .and_then(|project_dir| project_dir.as_str())
                        .filter(|project_dir| !project_dir.trim().is_empty())
                        .and_then(|project_dir| std::path::Path::new(project_dir).file_name())
                        .and_then(|project| project.to_str())
                        .filter(|project| !project.trim().is_empty())
                    {
                        projected.insert(
                            "project".to_string(),
                            serde_json::Value::String(project.to_string()),
                        );
                    }

                    for field in ["role", "bulletin"] {
                        if let Some(value) = session
                            .get(field)
                            .and_then(|value| value.as_str())
                            .filter(|value| !value.trim().is_empty())
                        {
                            projected.insert(
                                field.to_string(),
                                serde_json::Value::String(value.to_string()),
                            );
                        }
                    }

                    serde_json::Value::Object(projected)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    serde_json::json!({ "sessions": sessions })
}

async fn cli_post(path: &str, body: &serde_json::Value) -> anyhow::Result<()> {
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!("http://localhost:{port}{path}");
    let client = reqwest::Client::new();
    let resp = client.post(&url).json(body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    // Non-2xx must exit non-zero (like cli_delete): a rejected send that
    // prints its error but exits 0 reads as success to scripted callers,
    // which is the silent-failure shape task #1395 removes.
    let body = classify_http_response(status, &text)?;
    println!("{body}");
    Ok(())
}

async fn cli_delete(path: &str) -> anyhow::Result<()> {
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!("http://localhost:{port}{path}");
    let client = reqwest::Client::new();
    let resp = client.delete(&url).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    let body = classify_http_response(status, &text)?;
    println!("{body}");
    Ok(())
}

/// Classify an HTTP response into a CLI success-or-error.
///
/// Returns `Ok(body)` for 2xx, `Err` for everything else. The previous
/// behaviour in `cli_delete` printed the body and returned Ok for any
/// status, which made daemon 404s look like a silent success — half of
/// the silent-failure chain issue #646 is fixing.
///
/// Pulled out as a pure function so it is testable without a reqwest
/// round-trip; the HTTP-dependent parts (URL building, connecting,
/// body read) are orchestration, not logic.
fn classify_http_response(status: reqwest::StatusCode, body: &str) -> anyhow::Result<String> {
    if status.is_success() {
        Ok(body.to_string())
    } else if body.is_empty() {
        anyhow::bail!("request failed with HTTP {status}")
    } else {
        anyhow::bail!("request failed with HTTP {status}: {body}")
    }
}

/// Strip the leading `%` from a tmux pane id for wire transport.
///
/// Axum percent-decodes path segments, so placing a raw `%74` in the URL
/// arrives at the handler as `t` (0x74 == ASCII `t`) and silently 404s.
/// The canonical form on the wire is the numeric suffix only; the server
/// prepends `%` on receive (and tolerates `%` defensively). See issue #646.
fn pane_wire_suffix(pane: &str) -> &str {
    pane.strip_prefix('%').unwrap_or(pane)
}

/// Chars that must be percent-encoded to keep a string a single URL path
/// segment. Covers `/` (otherwise axum treats the segment as multiple),
/// `%` (otherwise axum misreads already-encoded sequences), `?` / `#`
/// (would start query/fragment), and the controls + space / quote / angle
/// brackets / backslash that URL parsers commonly disallow in paths.
const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'\\');

/// Percent-encode a string so it round-trips as a single URL path segment.
///
/// ouija session ids can legitimately contain `/` (e.g. branch-name-style
/// ids like `feat/646-...` pass through the session-spawn API unvalidated
/// and end up as `sender_id` for downstream commands). Interpolating them
/// raw into `/api/pane/{pane}/pending-replies/{from}` breaks axum's
/// single-segment match and silently 404s — the same failure class issue
/// #646 fixes for the pane segment.
fn encode_path_segment(segment: &str) -> String {
    percent_encoding::utf8_percent_encode(segment, PATH_SEGMENT).to_string()
}

fn read_one_shot_file(path: &std::path::Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).with_context(|| {
        format!(
            "failed to read UTF-8 one-shot prompt from {}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    fn persisted_dormant(
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

    fn subcommand_long_help(name: &str) -> String {
        let mut command = Cli::command()
            .find_subcommand(name)
            .unwrap_or_else(|| panic!("missing {name} subcommand"))
            .clone();
        let mut help = Vec::new();
        command
            .write_long_help(&mut help)
            .expect("render subcommand help");
        String::from_utf8(help).expect("clap help is UTF-8")
    }

    #[test]
    fn pane_wire_suffix_strips_leading_percent() {
        assert_eq!(pane_wire_suffix("%74"), "74");
    }

    #[test]
    fn pane_wire_suffix_leaves_bare_suffix_alone() {
        assert_eq!(pane_wire_suffix("74"), "74");
    }

    #[test]
    fn pane_wire_suffix_only_strips_first_percent() {
        // Defensive: if something handed us a doubly-prefixed form, we only
        // peel one layer. The server helper is equally tolerant, so either
        // `74` or `%74` resolves. A hypothetical `%%74` would stay `%74`
        // which the server still resolves correctly.
        assert_eq!(pane_wire_suffix("%%74"), "%74");
    }

    #[test]
    fn pane_wire_suffix_handles_empty_string() {
        assert_eq!(pane_wire_suffix(""), "");
    }

    #[test]
    fn recover_backend_identity_cli_requires_one_exact_public_id() {
        let cli =
            Cli::try_parse_from(["ouija", "recover-backend-identity", "divine-invite-darshan"])
                .expect("explicit recovery command must parse");

        match cli.command {
            Command::RecoverBackendIdentity { session_id } => {
                assert_eq!(session_id, "divine-invite-darshan");
            }
            _ => panic!("expected recover-backend-identity command"),
        }
        assert!(Cli::try_parse_from(["ouija", "recover-backend-identity"]).is_err());
    }

    #[test]
    fn backend_recovery_evidence_preserves_every_positive_signal_without_whoami_resolution() {
        let evidence = backend_recovery_caller_evidence(
            Some("%712".into()),
            Some("pane-owner".into()),
            Some("stale-env-owner".into()),
        );

        assert_eq!(evidence.pane.as_deref(), Some("%712"));
        assert_eq!(evidence.pane_var_id.as_deref(), Some("pane-owner"));
        assert_eq!(evidence.env_id.as_deref(), Some("stale-env-owner"));
    }

    #[test]
    fn spawn_session_cli_preserves_legacy_idle_policy_values() {
        let cli = Cli::try_parse_from([
            "ouija",
            "spawn-session",
            "worker",
            "--parent-session",
            "parent",
            "--idle-policy",
            "ask-parent-when-done",
        ])
        .expect("spawn-session lifecycle args parse");

        match cli.command {
            Command::SpawnSession {
                parent_session,
                no_parent_session,
                idle_policy,
                ..
            } => {
                assert_eq!(parent_session.as_deref(), Some("parent"));
                assert!(!no_parent_session);
                assert_eq!(
                    idle_policy,
                    Some(crate::daemon_protocol::IdlePolicy::AskParentWhenDone)
                );
            }
            _ => panic!("expected spawn-session command"),
        }
    }

    #[test]
    fn spawn_session_cli_accepts_a_positive_active_context_duration() {
        // Break caught: spawn-session must accept the opt-in active-context
        // policy instead of treating it as an unknown CLI argument.
        let parsed = Cli::try_parse_from([
            "ouija",
            "spawn-session",
            "worker",
            "--fresh-context-after-active",
            "1h",
            "--no-parent-session",
            "--when-done",
            "keep-open",
        ]);

        match parsed
            .expect("a positive active-context duration must parse")
            .command
        {
            Command::SpawnSession {
                fresh_context_after_active,
                ..
            } => assert_eq!(fresh_context_after_active, Some(3_600)),
            _ => panic!("expected spawn-session command"),
        }
    }

    #[test]
    fn spawn_session_help_explains_first_launch_prompt_and_active_context_setup() {
        // Break caught: agents previously tried a nonexistent `ouija spawn`
        // flow and expected a launch-only continuation on first creation.
        // The actual spawn-session help must describe its stored-prompt
        // contract and name the active-context setup action.
        let help = subcommand_long_help("spawn-session");

        assert!(help.to_lowercase().contains("complete bounded assignment"));
        assert!(help.contains("Bootstrap active-context refresh"));
        assert!(help.contains("no `ouija spawn` alias"));
        assert!(help.contains("does not support `--one-shot-file`"));
    }

    #[test]
    fn active_context_duration_parser_normalizes_supported_units() {
        // Break caught: changing a unit multiplier or passing the raw text to
        // the API would configure a different active-time limit than requested.
        assert_eq!(parse_fresh_context_after_active("1h"), Ok(3_600));
        assert_eq!(parse_fresh_context_after_active("90m"), Ok(5_400));
        assert_eq!(parse_fresh_context_after_active("3600s"), Ok(3_600));
    }

    #[test]
    fn active_context_duration_parser_rejects_invalid_or_non_positive_values() {
        // Break caught: malformed, zero, unitless, and overflowing values
        // must never enter the numeric API contract.
        assert_eq!(
            parse_fresh_context_after_active(""),
            Err("duration must be a positive whole number followed by h, m, or s".into())
        );
        assert_eq!(
            parse_fresh_context_after_active("0h"),
            Err("duration must be greater than zero".into())
        );
        assert_eq!(
            parse_fresh_context_after_active("1"),
            Err("duration must end with h, m, or s".into())
        );
        assert_eq!(
            parse_fresh_context_after_active("1.5h"),
            Err("duration must be a positive whole number followed by h, m, or s".into())
        );
        assert_eq!(
            parse_fresh_context_after_active("18446744073709551616s"),
            Err("duration value is too large".into())
        );
        assert_eq!(
            parse_fresh_context_after_active("5124095576030432h"),
            Err("duration overflows seconds".into())
        );
    }

    #[test]
    fn restart_session_cli_requires_fresh_for_active_context_duration() {
        // Break caught: the CLI must not send a policy-changing restart that
        // omits the required fresh context transition.
        let parsed = Cli::try_parse_from([
            "ouija",
            "restart-session",
            "worker",
            "--fresh-context-after-active",
            "1h",
        ]);

        assert!(parsed.is_err());
    }

    #[test]
    fn inject_only_task_cli_requires_and_preserves_exact_target() {
        let missing_target = Cli::try_parse_from([
            "ouija",
            "task",
            "add",
            "context-audit",
            "*/15 * * * *",
            "audit",
            "--inject-only",
        ]);
        assert!(missing_target.is_err());

        let cli = Cli::try_parse_from([
            "ouija",
            "task",
            "add",
            "context-audit",
            "*/15 * * * *",
            "audit",
            "--target",
            "manual-root",
            "--inject-only",
        ])
        .expect("inject-only task parses with exact target");

        match cli.command {
            Command::Task {
                action:
                    TaskAction::Add {
                        target,
                        inject_only,
                        ..
                    },
            } => {
                assert_eq!(target.as_deref(), Some("manual-root"));
                assert!(inject_only);
            }
            _ => panic!("expected task add command"),
        }
    }

    #[test]
    fn restart_session_cli_accepts_scoped_prompt_controls_and_backend() {
        let cli = Cli::try_parse_from([
            "ouija",
            "restart-session",
            "worker",
            "--prompt",
            "replacement",
            "--suppress-stored-prompt",
            "--one-shot-file",
            "/tmp/adopt.txt",
            "--backend",
            "codex-cli",
        ])
        .expect("restart prompt controls must parse");

        match cli.command {
            Command::RestartSession {
                prompt,
                suppress_stored_prompt,
                one_shot_file,
                backend,
                ..
            } => {
                assert_eq!(prompt.as_deref(), Some("replacement"));
                assert!(suppress_stored_prompt);
                assert_eq!(
                    one_shot_file.as_deref(),
                    Some(std::path::Path::new("/tmp/adopt.txt"))
                );
                assert_eq!(backend.as_deref(), Some("codex-cli"));
            }
            _ => panic!("expected restart-session command"),
        }
    }

    #[test]
    fn restart_session_help_separates_durable_prompt_from_launch_only_context() {
        // Break caught: bootstrap attempts reused transient handoff prose as
        // the stored prompt, repeated non-idempotent work after replay, or
        // omitted an explicit backend after identity evidence had become
        // unusable.
        let help = subcommand_long_help("restart-session");
        let spawn_help = subcommand_long_help("spawn-session");

        assert!(help.contains("durable stored base prompt"));
        assert!(help.contains("replayed by default after every fresh restart"));
        assert!(help.contains("re-entrant, state-checking"));
        assert!(help.contains("expensive, destructive, or external actions"));
        assert!(help.contains("current authorization"));
        assert!(help.contains("transient recovery prose"));
        assert!(help.contains("verified current-work continuation"));
        assert!(help.contains("binding is absent or cannot be trusted"));
        assert!(spawn_help.contains("re-entrant, state-checking"));
        assert!(spawn_help.contains("perform only remaining work"));
        assert!(spawn_help.contains("destructive or external actions"));
        assert!(spawn_help.contains("current authorization"));
    }

    #[test]
    fn one_shot_file_reader_accepts_utf8_and_rejects_invalid_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let valid = dir.path().join("valid.txt");
        std::fs::write(&valid, "verify live state").unwrap();
        assert_eq!(read_one_shot_file(&valid).unwrap(), "verify live state");

        let invalid = dir.path().join("invalid.txt");
        std::fs::write(&invalid, [0xff, 0xfe]).unwrap();
        let error = read_one_shot_file(&invalid).unwrap_err().to_string();
        assert!(error.contains("UTF-8 one-shot prompt"));

        let missing = dir.path().join("missing.txt");
        let error = read_one_shot_file(&missing).unwrap_err().to_string();
        assert!(error.contains(missing.to_string_lossy().as_ref()));
    }

    #[test]
    fn compact_session_list_preserves_decimal_incarnation() {
        let projected = project_session_list(&serde_json::json!({
            "sessions": [{
                "id": "worker",
                "origin": "local",
                "session_incarnation": "18446744073709551615"
            }]
        }));
        assert_eq!(
            projected["sessions"][0]["session_incarnation"],
            "18446744073709551615"
        );
    }

    #[test]
    fn rollover_cli_requires_stdin_and_accepts_expired_replacement() {
        assert!(Cli::try_parse_from(["ouija", "rollover", "prepare"]).is_err());
        let cli = Cli::try_parse_from([
            "ouija",
            "rollover",
            "prepare",
            "--stdin",
            "--replace-expired",
        ])
        .unwrap();
        match cli.command {
            Command::Rollover {
                action:
                    RolloverAction::Prepare {
                        stdin,
                        replace_expired,
                    },
            } => {
                assert!(stdin);
                assert!(replace_expired);
            }
            _ => panic!("expected rollover prepare"),
        }

        let cli = Cli::try_parse_from(["ouija", "rollover", "adopt", "opaque-token"]).unwrap();
        match cli.command {
            Command::Rollover {
                action: RolloverAction::Adopt { token },
            } => assert_eq!(token, "opaque-token"),
            _ => panic!("expected rollover adopt"),
        }

        let cli = Cli::try_parse_from(["ouija", "rollover", "cleanup", "--force-pending"]).unwrap();
        match cli.command {
            Command::Rollover {
                action: RolloverAction::Cleanup { force_pending },
            } => assert!(force_pending),
            _ => panic!("expected rollover cleanup"),
        }
    }

    #[test]
    fn rollover_owner_parser_requires_exact_local_string_incarnation() {
        let session = serde_json::json!({
            "id": "worker",
            "origin": "local",
            "session_incarnation": "18446744073709551615",
        });
        assert_eq!(
            rollover_incarnation_from_session(&session, "worker").unwrap(),
            u64::MAX
        );
        assert!(rollover_incarnation_from_session(&session, "other").is_err());

        let mut remote = session.clone();
        remote["origin"] = serde_json::json!("remote");
        assert!(rollover_incarnation_from_session(&remote, "worker").is_err());

        let mut numeric = session;
        numeric["session_incarnation"] = serde_json::json!(7);
        assert!(rollover_incarnation_from_session(&numeric, "worker").is_err());
    }

    #[test]
    fn rollover_incarnation_evidence_rejects_absent_unbound_caller() {
        let error = verify_rollover_incarnation_evidence(None, false, 9)
            .unwrap_err()
            .to_string();
        assert!(error.contains("caller-owned incarnation evidence"));
    }

    #[test]
    fn rollover_incarnation_evidence_rejects_stale_hint() {
        let error = verify_rollover_incarnation_evidence(Some(8), false, 9)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not match"));
    }

    #[test]
    fn rollover_incarnation_evidence_accepts_exact_hint_or_bound_backend() {
        assert_eq!(
            verify_rollover_incarnation_evidence(Some(9), false, 9).unwrap(),
            9
        );
        assert_eq!(
            verify_rollover_incarnation_evidence(None, true, 9).unwrap(),
            9
        );
    }

    #[test]
    fn spawn_session_cli_accepts_primary_when_done_values() {
        for (value, expected) in [
            ("keep-open", IdlePolicy::KeepOpen),
            ("ask-parent", IdlePolicy::AskParentWhenDone),
            ("close", IdlePolicy::CloseWhenDone),
        ] {
            let cli = Cli::try_parse_from([
                "ouija",
                "spawn-session",
                "worker",
                "--parent-session",
                "parent",
                "--when-done",
                value,
            ])
            .unwrap_or_else(|error| panic!("--when-done {value} must parse: {error}"));

            match cli.command {
                Command::SpawnSession {
                    when_done,
                    idle_policy,
                    ..
                } => {
                    assert_eq!(when_done.map(IdlePolicy::from), Some(expected));
                    assert_eq!(idle_policy, None);
                }
                _ => panic!("expected spawn-session command"),
            }
        }
    }

    #[test]
    fn spawn_session_cli_rejects_both_completion_flags() {
        let error = Cli::try_parse_from([
            "ouija",
            "spawn-session",
            "worker",
            "--no-parent-session",
            "--when-done",
            "keep-open",
            "--idle-policy",
            "keep-open",
        ])
        .err()
        .expect("completion flags must conflict")
        .to_string();

        assert!(error.contains("--when-done"));
        assert!(error.contains("--idle-policy"));
        assert!(
            error.contains("cannot be used with"),
            "error must explain the conflict, got: {error}"
        );
    }

    #[test]
    fn spawn_session_help_documents_primary_and_deprecated_completion_flags() {
        use clap::CommandFactory;

        let mut cmd = Cli::command();
        let spawn_session = cmd
            .find_subcommand_mut("spawn-session")
            .expect("spawn-session subcommand exists");
        let mut help = Vec::new();
        spawn_session.write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        assert!(help.contains("--when-done <WHEN_DONE>"));
        for value in ["keep-open", "ask-parent", "close"] {
            assert!(
                help.contains(value),
                "primary completion value {value} missing from help:\n{help}"
            );
        }
        assert!(help.contains("--idle-policy <IDLE_POLICY>"));
        assert!(
            help.contains("Deprecated"),
            "legacy flag must be marked deprecated:\n{help}"
        );
        for value in ["keep-open", "ask-parent-when-done", "close-when-done"] {
            assert!(
                help.contains(value),
                "legacy completion value {value} missing from help:\n{help}"
            );
        }
    }

    #[test]
    fn spawn_session_cli_rejects_manual_clear_reminder_commands() {
        let error = Cli::try_parse_from([
            "ouija",
            "spawn-session",
            "worker",
            "--no-parent-session",
            "--idle-policy",
            "keep-open",
            "--reminder",
            "When done, run ouija clear-reminder 7",
        ])
        .err()
        .expect("manual clear-reminder instructions must be rejected")
        .to_string();

        assert!(error.contains("ouija clear-reminder"));
        assert!(
            error.contains("generated"),
            "error must explain that Ouija supplies the command, got: {error}"
        );
    }

    #[test]
    fn restart_session_cli_rejects_manual_clear_reminder_commands() {
        let error = Cli::try_parse_from([
            "ouija",
            "restart-session",
            "worker",
            "--reminder",
            "When done, run ouija clear-reminder 7",
        ])
        .err()
        .expect("manual clear-reminder instructions must be rejected");

        assert!(error.to_string().contains("ouija clear-reminder"));
    }

    #[test]
    fn ask_cli_accepts_stdin_without_message_argument() {
        let cli = Cli::try_parse_from(["ouija", "ask", "parent", "--stdin", "--from", "worker"])
            .expect("ask --stdin parses without positional message");

        match cli.command {
            Command::Ask {
                to,
                message,
                stdin,
                message_file,
                from,
            } => {
                assert_eq!(to, "parent");
                assert_eq!(message, None);
                assert!(stdin);
                assert_eq!(message_file, None);
                assert_eq!(from.as_deref(), Some("worker"));
            }
            _ => panic!("expected ask command"),
        }
    }

    #[test]
    fn spawn_lifecycle_validation_teaches_missing_parent_choice() {
        let err = validate_spawn_lifecycle(
            None,
            false,
            Some(&crate::daemon_protocol::IdlePolicy::KeepOpen),
        )
        .unwrap_err();

        assert!(
            err.contains("--parent-session <SESSION_ID>"),
            "error must teach parent-session choice, got: {err}"
        );
        assert!(
            err.contains("--no-parent-session"),
            "error must teach no-parent-session choice, got: {err}"
        );
    }

    #[test]
    fn spawn_lifecycle_validation_teaches_missing_idle_policy() {
        let err = validate_spawn_lifecycle(None, true, None).unwrap_err();

        assert!(
            err.contains("--when-done <keep-open|ask-parent|close>"),
            "error must teach when-done choices, got: {err}"
        );
    }

    // --- encode_path_segment (issue #646 review follow-up) ---
    //
    // Sender ids in ouija can contain `/` (branch-name-style ids like
    // `feat/646-...` are accepted by the session spawn API and end up flowing
    // into `sender_id` for `ouija clear-reply`). Interpolating them raw into
    // `/api/pane/{pane}/pending-replies/{from}` breaks axum's single-segment
    // match and silently 404s. The CLI must percent-encode the sender_id
    // segment before building the URL.

    #[test]
    fn encode_path_segment_encodes_slashes() {
        assert_eq!(
            encode_path_segment("feat/646-foo"),
            "feat%2F646-foo",
            "`/` must be percent-encoded so the URL stays a single path segment"
        );
    }

    #[test]
    fn encode_path_segment_passes_through_common_session_chars() {
        // Alphanumerics plus the typical separators used in ouija session ids
        // (hyphens and underscores) must round-trip unchanged for legibility
        // in logs and audit trails.
        assert_eq!(encode_path_segment("my-session_42"), "my-session_42");
    }

    #[test]
    fn encode_path_segment_encodes_percent_literal() {
        // A caller sending a literal `%` (e.g. an id containing `100%`)
        // must come out as `%25` so axum decodes it back to `%`.
        assert_eq!(encode_path_segment("100%"), "100%25");
    }

    #[test]
    fn encode_path_segment_encodes_space_and_hash() {
        // Control chars and `#` / `?` would terminate the path segment or
        // start a query string / fragment on the wire; encode them.
        assert_eq!(encode_path_segment("a b#c?d"), "a%20b%23c%3Fd");
    }

    #[test]
    fn clear_reminder_help_mentions_from_option() {
        use clap::CommandFactory;

        let mut cmd = Cli::command();
        let clear_reminder = cmd
            .find_subcommand_mut("clear-reminder")
            .expect("clear-reminder subcommand exists");
        let mut help = Vec::new();
        clear_reminder.write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        assert!(
            help.contains("Usage: clear-reminder [OPTIONS] <CLEARING_ID>"),
            "clear-reminder usage must advertise options, got:\n{help}"
        );
        assert!(
            help.contains("--from <FROM>"),
            "clear-reminder help must advertise explicit sender support, got:\n{help}"
        );
    }

    #[test]
    fn clear_reminder_parses_explicit_from_option() {
        let cli = Cli::try_parse_from([
            "ouija",
            "clear-reminder",
            "42",
            "--from",
            "feat/62-add-from-support-to-ouija-clear-reminder",
        ])
        .unwrap();

        match cli.command {
            Command::ClearReminder { clearing_id, from } => {
                assert_eq!(clearing_id, 42);
                assert_eq!(
                    from.as_deref(),
                    Some("feat/62-add-from-support-to-ouija-clear-reminder")
                );
            }
            _ => panic!("expected clear-reminder command"),
        }
    }

    #[test]
    fn rename_parses_explicit_from_option() {
        let cli = Cli::try_parse_from(["ouija", "rename", "hub-cx", "--from", "hub"]).unwrap();

        match cli.command {
            Command::Rename { new_id, from } => {
                assert_eq!(new_id, "hub-cx");
                assert_eq!(from.as_deref(), Some("hub"));
            }
            _ => panic!("expected rename command"),
        }
    }

    // --- classify_http_response (issue #646 review follow-up) ---
    //
    // cli_delete (and any future HTTP helper built on top of this) must
    // surface non-2xx responses as hard errors so the CLI exits non-zero.
    // The previous behaviour — print the body and exit 0 for any status —
    // made 404s from the daemon look like success, which is half of the
    // silent-failure chain this PR is fixing.

    #[test]
    fn classify_http_response_success_returns_body() {
        use reqwest::StatusCode;
        let out = classify_http_response(StatusCode::OK, "{\"ok\":true}").unwrap();
        assert_eq!(out, "{\"ok\":true}");
    }

    #[test]
    fn classify_http_response_2xx_range_all_pass() {
        use reqwest::StatusCode;
        for code in [
            StatusCode::OK,
            StatusCode::CREATED,
            StatusCode::ACCEPTED,
            StatusCode::NO_CONTENT,
        ] {
            assert!(
                classify_http_response(code, "").is_ok(),
                "{code} must be classified as success"
            );
        }
    }

    // --- pick_session_id (issue #42) ---
    //
    // Precedence regression: $OUIJA_SESSION_ID is exported into spawned panes
    // via `tmux new-window -e KEY=VAL` and cannot be updated once the shell
    // is running. When a pane outlives its originating session and gets
    // re-registered to a different ouija id, the env var stays stale while
    // the daemon-controlled @ouija_session pane var is current. The pane var
    // must outrank the env var so peers don't reject calls under a stale id.

    #[test]
    fn pick_session_id_prefers_pane_var_over_env_var_in_tmux() {
        // In a tmux pane, the daemon-controlled pane var is authoritative.
        let res = pick_session_id(
            Some("%74"),
            Some("keycast".into()),
            Some("feat/95-stale".into()),
        );
        assert_eq!(
            res,
            SessionIdResolution::Found("keycast".into(), IdentitySource::PaneVar)
        );
    }

    #[test]
    fn pick_session_id_falls_back_to_env_var_when_pane_var_missing() {
        // Race window before the daemon's SetTmuxVar effect lands, or
        // opencode subshell that lost TMUX_PANE inheritance — env var is the
        // only signal pointing at the right session.
        let res = pick_session_id(Some("%74"), None, Some("keycast".into()));
        assert_eq!(
            res,
            SessionIdResolution::Found("keycast".into(), IdentitySource::EnvVar)
        );
    }

    #[test]
    fn pick_session_id_treats_empty_pane_var_as_absent() {
        let res = pick_session_id(Some("%74"), Some("".into()), Some("env-id".into()));
        assert_eq!(
            res,
            SessionIdResolution::Found("env-id".into(), IdentitySource::EnvVar)
        );
    }

    #[test]
    fn pick_session_id_falls_through_to_pane_lookup_when_neither_signal_set() {
        let res = pick_session_id(Some("%74"), None, None);
        assert_eq!(res, SessionIdResolution::LookupByPane("%74".into()));
    }

    #[test]
    fn pick_session_id_outside_tmux_uses_env_var() {
        // Non-tmux callers (opencode HTTP API plugin, scripts) have no pane
        // var to consult — env var is the only signal.
        let res = pick_session_id(None, None, Some("opencode-session".into()));
        assert_eq!(
            res,
            SessionIdResolution::Found("opencode-session".into(), IdentitySource::EnvVar)
        );
    }

    #[test]
    fn pick_session_id_outside_tmux_with_no_env_var_returns_none() {
        // No tmux pane, no env var — caller must pass --from <id> explicitly.
        let res = pick_session_id(None, None, None);
        assert_eq!(res, SessionIdResolution::None);
    }

    #[test]
    fn pick_session_id_outside_tmux_ignores_stray_pane_var_input() {
        // Defensive: if a caller somehow supplies a pane var without a pane
        // (an internally inconsistent state), we don't trust it — the pane
        // var without a pane id can't be the daemon-controlled signal we
        // claim it is. Fall through to env var.
        let res = pick_session_id(None, Some("ghost".into()), Some("real".into()));
        assert_eq!(
            res,
            SessionIdResolution::Found("real".into(), IdentitySource::EnvVar)
        );
    }

    #[test]
    fn explicit_sender_remains_authoritative_across_backend_thread_rollover() {
        let sender = resolve_explicit_sender(
            "hub-4".into(),
            Some("%replacement".into()),
            None,
            None,
            Some(backend::BackendSessionIdentity {
                backend: "codex-cli".into(),
                session_id: "new-thread".into(),
            }),
        );

        assert_eq!(sender.id, "hub-4");
        assert_eq!(sender.context["trusted_local_claim"], true);
        assert_eq!(sender.context["pane"], "%replacement");
        assert!(sender.context["self_id"].is_null());
        assert_eq!(
            sender.context["backend_identity"],
            serde_json::json!({
                "backend": "codex-cli",
                "session_id": "new-thread",
            })
        );
    }

    #[test]
    fn explicit_sender_preserves_registered_env_observation_for_daemon_validation() {
        let sender =
            resolve_explicit_sender("hub-4".into(), None, None, Some("sibling".into()), None);

        assert_eq!(sender.id, "hub-4");
        assert_eq!(sender.context["self_id"], "sibling");
        assert_eq!(sender.context["trusted_local_claim"], true);
    }

    #[test]
    fn backend_canonical_identity_rejects_a_conflicting_local_signal() {
        let err = arbitrate_backend_identity(
            Some(("stale-pane-id".into(), IdentitySource::PaneVar)),
            Some("canonical-backend-id".into()),
        )
        .unwrap_err();

        assert!(err.local_id.contains("stale-pane-id"));
        assert!(err.canonical_id.contains("canonical-backend-id"));
    }

    #[test]
    fn backend_canonical_identity_is_reported_when_local_signal_agrees() {
        let resolved = arbitrate_backend_identity(
            Some(("canonical-backend-id".into(), IdentitySource::EnvVar)),
            Some("canonical-backend-id".into()),
        )
        .unwrap();

        assert_eq!(
            resolved,
            Some((
                "canonical-backend-id".into(),
                IdentitySource::BackendIdentity
            ))
        );
    }

    #[test]
    fn incomplete_backend_identity_does_not_strand_verified_local_identity() {
        let local = ("hub".into(), IdentitySource::PaneVar);
        let backend_canonical = backend_canonical_for_arbitration(
            Some(&local),
            Err(BackendIdentityLookupError::daemon_rejection(
                "incomplete_legacy",
                "legacy backend metadata is incomplete",
            )),
        )
        .unwrap();

        let resolved = arbitrate_backend_identity(Some(local), backend_canonical).unwrap();

        assert_eq!(
            resolved,
            Some(("hub".into(), IdentitySource::PaneVar)),
            "a non-canonical incomplete row cannot disprove a verified local identity"
        );
    }

    #[test]
    fn incomplete_backend_identity_does_not_strand_registered_env_identity() {
        let local = ("hub".into(), IdentitySource::EnvVar);
        let backend_canonical = backend_canonical_for_arbitration(
            Some(&local),
            Err(BackendIdentityLookupError::daemon_rejection(
                "incomplete_legacy",
                "legacy backend metadata is incomplete",
            )),
        )
        .unwrap();

        let resolved = arbitrate_backend_identity(Some(local), backend_canonical).unwrap();

        assert_eq!(resolved, Some(("hub".into(), IdentitySource::EnvVar)));
    }

    #[test]
    fn incomplete_backend_identity_without_local_proof_remains_terminal() {
        let error = backend_canonical_for_arbitration(
            None,
            Err(BackendIdentityLookupError::daemon_rejection(
                "incomplete_legacy",
                "legacy backend metadata is incomplete",
            )),
        )
        .unwrap_err();

        assert_eq!(error.outcome.as_deref(), Some("incomplete_legacy"));
    }

    #[test]
    fn non_incomplete_backend_failures_remain_terminal_with_local_hint() {
        let local = ("hub".into(), IdentitySource::PaneLookup);
        for outcome in ["ambiguous", "not_found"] {
            let error = backend_canonical_for_arbitration(
                Some(&local),
                Err(BackendIdentityLookupError::daemon_rejection(
                    outcome,
                    "backend identity has no safe canonical owner",
                )),
            )
            .unwrap_err();

            assert_eq!(error.outcome.as_deref(), Some(outcome));
        }

        let transport_error = backend_canonical_for_arbitration(
            Some(&local),
            Err(BackendIdentityLookupError::protocol_failure(
                "daemon unreachable",
            )),
        )
        .unwrap_err();
        assert!(transport_error.outcome.is_none());
    }

    #[test]
    fn resolve_message_accepts_positional_text() {
        let message = resolve_message(Some("hello `literal`".into()), false, None).unwrap();
        assert_eq!(message, "hello `literal`");
    }

    #[test]
    fn resolve_message_accepts_file_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("message.txt");
        std::fs::write(&path, "hello $(literal)\n").unwrap();

        let message = resolve_message(None, false, Some(path)).unwrap();

        assert_eq!(message, "hello $(literal)\n");
    }

    #[test]
    fn resolve_message_rejects_missing_source() {
        let err = resolve_message(None, false, None).unwrap_err();

        assert!(err.to_string().contains("provide a message argument"));
    }

    #[test]
    fn resolve_message_rejects_multiple_sources() {
        let err = resolve_message(Some("hello".into()), true, None).unwrap_err();

        assert!(err.to_string().contains("provide only one message source"));
    }

    #[test]
    fn classify_http_response_404_surfaces_as_error() {
        use reqwest::StatusCode;
        let err = classify_http_response(
            StatusCode::NOT_FOUND,
            "{\"error\":\"pane 'x' is not registered\"}",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("404"),
            "error message must include the status code, got: {msg}"
        );
        assert!(
            msg.contains("pane 'x' is not registered"),
            "error message must include the response body, got: {msg}"
        );
    }

    #[test]
    fn classify_http_response_500_surfaces_as_error() {
        use reqwest::StatusCode;
        let err = classify_http_response(StatusCode::INTERNAL_SERVER_ERROR, "boom").unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[test]
    fn classify_http_response_400_with_empty_body_still_errors() {
        // Empty body must not swallow the error — status alone is sufficient.
        use reqwest::StatusCode;
        let err = classify_http_response(StatusCode::BAD_REQUEST, "").unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[test]
    fn session_list_projection_keeps_discovery_fields_only() {
        let status = serde_json::json!({
            "daemon": "locota",
            "transports": [{"name": "nostr", "ready": true}],
            "assistant_panes": [{"pane_id": "%1", "session": "ouija"}],
            "sessions": [{
                "id": "ouija-next-issue",
                "origin": "local",
                "session_incarnation": "42",
                "parent_session": "hub-cx",
                "project_dir": "/home/daniel/code/ouija",
                "role": "working on ouija",
                "bulletin": "ready",
                "stale": true,
                "worktree_present": true,
                "fresh_context_after_active_secs": 3600,
                "active_context_accumulated_secs": 1234,
                "active_context_segment_open": true,
                "active_context_restart_due": true,
                "prompt": "internal prompt that should not be listed",
                "reminder": "internal reminder that should not be listed",
                "backend_session_id": "ses_secret_internal",
                "iteration_log": ["noise"]
            }]
        });

        let projected = project_session_list(&status);

        assert_eq!(
            projected,
            serde_json::json!({
                "sessions": [{
                    "id": "ouija-next-issue",
                    "origin": "local",
                    "session_incarnation": "42",
                    "parent_session": "hub-cx",
                    "project": "ouija",
                    "role": "working on ouija",
                    "bulletin": "ready"
                }]
            })
        );
        assert!(projected.get("daemon").is_none());
        assert!(projected["sessions"][0].get("project_dir").is_none());
        assert!(projected["sessions"][0].get("stale").is_none());
        assert!(projected["sessions"][0].get("worktree_present").is_none());
        assert!(projected["sessions"][0].get("prompt").is_none());
        assert!(projected["sessions"][0].get("reminder").is_none());
        assert!(projected["sessions"][0].get("backend_session_id").is_none());
        assert!(
            projected["sessions"][0]
                .get("fresh_context_after_active_secs")
                .is_none()
        );
        assert!(
            projected["sessions"][0]
                .get("active_context_accumulated_secs")
                .is_none()
        );
        assert!(
            projected["sessions"][0]
                .get("active_context_segment_open")
                .is_none()
        );
        assert!(
            projected["sessions"][0]
                .get("active_context_restart_due")
                .is_none()
        );
    }

    #[test]
    fn session_list_projection_omits_empty_optional_discovery_fields() {
        let status = serde_json::json!({
            "sessions": [{
                "id": "quiet-session",
                "origin": "remote:locota",
                "project_dir": null,
                "role": "",
                "bulletin": "   "
            }]
        });

        let projected = project_session_list(&status);

        assert_eq!(
            projected,
            serde_json::json!({
                "sessions": [{
                    "id": "quiet-session",
                    "origin": "remote:locota",
                    "parent_session": null
                }]
            })
        );
    }

    #[test]
    fn metadata_for_restored_session_preserves_persisted_fields() {
        let metadata = crate::state::SessionMetadata {
            project_dir: Some("/tmp/project".into()),
            canonical_project_identity: Some("/tmp/project".into()),
            role: Some("worker".into()),
            bulletin: Some("busy".into()),
            networked: false,
            worktree: true,
            vim_mode: true,
            backend_session_id: Some("ses_old".into()),
            backend: Some("opencode".into()),
            opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
            restart_generation: 9,
            backend_repair_reservation: None,
            session_incarnation: crate::daemon_protocol::SessionIncarnation(13),
            project_description: Some("project".into()),
            last_metadata_update: chrono::DateTime::from_timestamp(1_700_000_001, 0),
            model: Some("openrouter/sonnet".into()),
            effort: Some("high".into()),
            codex_home: None,
            reminder: Some("keep going".into()),
            parent_session: Some("parent".into()),
            idle_policy: Some(crate::daemon_protocol::IdlePolicy::CloseWhenDone),
            prompt: Some("initial prompt".into()),
            iteration: 4,
            iteration_log: vec![],
            last_iteration_at: Some(1_700_000_002),
            on_fire: Some(crate::scheduler::OnFire::ContinueSession),
            worktree_present: Some(true),
            fresh_context_after_active_secs: Some(3_600),
            active_context_accumulated_secs: 1_234,
            active_context_segment_started_at: Some(1_700_000_003),
            active_context_restart_due: true,
            active_context_accounting_provisional: true,
        };

        let restored = metadata_for_restored_session(&metadata);

        assert_eq!(restored.project_dir, metadata.project_dir);
        assert_eq!(restored.role, metadata.role);
        assert_eq!(restored.bulletin, metadata.bulletin);
        assert_eq!(restored.networked, metadata.networked);
        assert_eq!(restored.worktree, metadata.worktree);
        assert_eq!(restored.vim_mode, metadata.vim_mode);
        assert_eq!(restored.backend_session_id, metadata.backend_session_id);
        assert_eq!(restored.backend, metadata.backend);
        assert_eq!(restored.opencode_binding, metadata.opencode_binding);
        assert_eq!(restored.restart_generation, metadata.restart_generation);
        assert_eq!(restored.session_incarnation, metadata.session_incarnation);
        assert_eq!(restored.project_description, metadata.project_description);
        assert_eq!(restored.last_metadata_update, Some(1_700_000_001));
        assert_eq!(restored.model, metadata.model);
        assert_eq!(restored.effort, metadata.effort);
        assert_eq!(restored.reminder, metadata.reminder);
        assert_eq!(restored.parent_session, metadata.parent_session);
        assert_eq!(restored.idle_policy, metadata.idle_policy);
        assert_eq!(restored.prompt, metadata.prompt);
        assert_eq!(restored.iteration, metadata.iteration);
        assert_eq!(restored.iteration_log, metadata.iteration_log);
        assert_eq!(restored.last_iteration_at, metadata.last_iteration_at);
        assert_eq!(restored.on_fire, metadata.on_fire);
        assert_eq!(restored.worktree_present, metadata.worktree_present);
        assert_eq!(
            restored.fresh_context_after_active_secs,
            metadata.fresh_context_after_active_secs
        );
        assert_eq!(
            restored.active_context_accumulated_secs,
            metadata.active_context_accumulated_secs
        );
        assert_eq!(
            restored.active_context_segment_started_at,
            metadata.active_context_segment_started_at
        );
        assert_eq!(
            restored.active_context_restart_due,
            metadata.active_context_restart_due
        );
        assert_eq!(
            restored.active_context_accounting_provisional,
            metadata.active_context_accounting_provisional
        );
    }

    #[tokio::test]
    async fn restore_persisted_dormant_sessions_before_live_reconciliation() {
        let dir = tempfile::tempdir().unwrap();
        let dormant = persisted_dormant(
            "rootfix",
            42,
            "codex-cli",
            "thread-rootfix",
            "/tmp/worktrees/rootfix",
            "/tmp/repository",
        );
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![],
            std::collections::BTreeMap::from([("rootfix".into(), dormant.clone())]),
            crate::daemon_protocol::SessionIncarnation(41),
            std::collections::BTreeMap::new(),
        );
        crate::persistence::save_sessions(dir.path(), &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "restore-dormant-test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });

        restore_persisted_sessions(&state).await.unwrap();

        let protocol = state.protocol.read().await;
        assert_eq!(protocol.dormant_sessions["rootfix"], dormant);
        assert!(protocol.sessions.is_empty());
        assert_eq!(
            protocol.incarnation_high_water,
            crate::daemon_protocol::SessionIncarnation(42)
        );
    }

    #[tokio::test]
    async fn restore_persisted_dormant_backend_suppresses_abandoned_abort() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let abort_port = listener.local_addr().unwrap().port();
        let daemon_port = abort_port.checked_sub(320).unwrap();
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let calls_for_route = calls.clone();
        let app = Router::new().route(
            "/session/{session_id}/abort",
            post(move || {
                let calls = calls_for_route.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dormant = persisted_dormant(
            "rootfix",
            42,
            "opencode",
            "ses_rootfix",
            "/tmp/worktrees/rootfix",
            "/tmp/repository",
        );
        let owner = dormant.prior_owner.clone();
        let lease = crate::daemon_protocol::LifecycleLease {
            owner: owner.clone(),
            phase: crate::daemon_protocol::LifecyclePhase::Stopping,
            backend: Some("opencode".into()),
            backend_session_id: Some("ses_rootfix".into()),
            backend_session_owner: Some(owner.clone()),
            restart_target_owner: None,
            restart_previous: None,
            project_dir: None,
            project_dir_owner: None,
            project_dir_cleanup_on_abandon: false,
            inert_pane: None,
            inert_pane_owner: None,
        };
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![],
            std::collections::BTreeMap::from([("rootfix".into(), dormant)]),
            owner.incarnation,
            std::collections::BTreeMap::from([("rootfix".into(), lease)]),
        );
        crate::persistence::save_sessions(dir.path(), &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "restore-dormant-backend-test".into(),
            npub: "npub1test".into(),
            port: daemon_port,
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });

        restore_persisted_sessions(&state).await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            state
                .protocol
                .read()
                .await
                .dormant_sessions
                .contains_key("rootfix")
        );
        assert!(
            crate::persistence::load_sessions(dir.path())
                .unwrap()
                .lifecycle_leases
                .is_empty()
        );
        server.abort();
    }

    #[tokio::test]
    async fn restore_persisted_dormant_worktree_suppresses_abandoned_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let worktree = repo.join(".ouija/worktrees/rootfix");
        let data_dir = root.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let run_git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_git(&["init", "-b", "main", repo.to_str().unwrap()]);
        run_git(&[
            "-C",
            repo.to_str().unwrap(),
            "config",
            "user.email",
            "test@example.com",
        ]);
        run_git(&["-C", repo.to_str().unwrap(), "config", "user.name", "Test"]);
        std::fs::write(repo.join("tracked"), "base\n").unwrap();
        run_git(&["-C", repo.to_str().unwrap(), "add", "tracked"]);
        run_git(&["-C", repo.to_str().unwrap(), "commit", "-m", "initial"]);
        run_git(&[
            "-C",
            repo.to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "rootfix",
            worktree.to_str().unwrap(),
        ]);

        let actual = crate::state::project_dir_identity(worktree.to_str().unwrap());
        let canonical = crate::state::project_dir_identity(repo.to_str().unwrap());
        let dormant = persisted_dormant(
            "rootfix",
            42,
            "codex-cli",
            "thread-rootfix",
            &actual,
            &canonical,
        );
        let owner = dormant.prior_owner.clone();
        let lease = crate::daemon_protocol::LifecycleLease {
            owner: owner.clone(),
            phase: crate::daemon_protocol::LifecyclePhase::Stopping,
            backend: None,
            backend_session_id: None,
            backend_session_owner: None,
            restart_target_owner: None,
            restart_previous: None,
            project_dir: Some(actual),
            project_dir_owner: Some(owner.clone()),
            project_dir_cleanup_on_abandon: true,
            inert_pane: None,
            inert_pane_owner: None,
        };
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![],
            std::collections::BTreeMap::from([("rootfix".into(), dormant)]),
            owner.incarnation,
            std::collections::BTreeMap::from([("rootfix".into(), lease)]),
        );
        crate::persistence::save_sessions(&data_dir, &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "restore-dormant-worktree-test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: data_dir.clone(),
            config_dir: data_dir.clone(),
        });

        restore_persisted_sessions(&state).await.unwrap();

        assert!(
            worktree.join("tracked").is_file(),
            "dormant ownership must preserve the parked worktree"
        );
        assert!(
            state
                .protocol
                .read()
                .await
                .dormant_sessions
                .contains_key("rootfix")
        );
        assert!(
            crate::persistence::load_sessions(&data_dir)
                .unwrap()
                .lifecycle_leases
                .is_empty()
        );
    }

    #[tokio::test]
    async fn restore_persisted_paneless_strong_opencode_session_spawns_activity_receiver() {
        // Break caught: startup rehydration bypasses Event::Register, so it
        // must explicitly recreate the exact optional-pane activity receiver.
        let dir = tempfile::tempdir().unwrap();
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![crate::persistence::PersistedSession {
                id: "restored-paneless".into(),
                pane: None,
                registered_at: chrono::Utc::now(),
                last_activity_at: chrono::Utc::now(),
                metadata: crate::state::SessionMetadata {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_restored_paneless".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    session_incarnation: crate::daemon_protocol::SessionIncarnation(42),
                    fresh_context_after_active_secs: Some(60),
                    ..Default::default()
                },
            }],
            std::collections::BTreeMap::new(),
            crate::daemon_protocol::SessionIncarnation(42),
            std::collections::BTreeMap::new(),
        );
        crate::persistence::save_sessions(dir.path(), &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "restore-paneless-agent-test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });

        restore_persisted_sessions(&state).await.unwrap();
        let owner = state.protocol.read().await.sessions["restored-paneless"].owner();
        assert!(
            state
                .notify_agent_owned(&owner, crate::session_agent::SessionMsg::Active)
                .await,
            "restored paneless owner must have a live activity receiver"
        );
        state.query_agent_pending_replies("restored-paneless").await;
        assert!(
            state.protocol.read().await.sessions["restored-paneless"]
                .metadata
                .active_context_segment_started_at
                .is_some()
        );
    }

    #[tokio::test]
    async fn restore_releases_paneless_worktree_lease_without_harming_shared_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let owner = crate::daemon_protocol::ResourceOwner {
            session_id: "pending".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(49),
        };
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![crate::persistence::PersistedSession {
                id: "live".into(),
                pane: None,
                registered_at: chrono::Utc::now(),
                last_activity_at: chrono::Utc::now(),
                metadata: crate::state::SessionMetadata {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_live".into()),
                    session_incarnation: crate::daemon_protocol::SessionIncarnation(48),
                    project_dir: Some("/tmp/.ouija/worktrees/project/shared-replacement".into()),
                    ..Default::default()
                },
            }],
            std::collections::BTreeMap::new(),
            crate::daemon_protocol::SessionIncarnation(50),
            std::collections::BTreeMap::from([(
                owner.session_id.clone(),
                crate::daemon_protocol::LifecycleLease {
                    owner: owner.clone(),
                    phase: crate::daemon_protocol::LifecyclePhase::Starting,
                    backend: None,
                    backend_session_id: None,
                    backend_session_owner: None,
                    restart_target_owner: None,
                    restart_previous: None,
                    project_dir: Some("/tmp/.ouija/worktrees/project/shared-replacement".into()),
                    project_dir_owner: Some(owner.clone()),
                    project_dir_cleanup_on_abandon: true,
                    inert_pane: None,
                    inert_pane_owner: None,
                },
            )]),
        );
        crate::persistence::save_sessions(dir.path(), &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });

        restore_persisted_sessions(&state).await.unwrap();

        let restored_snapshot = crate::persistence::load_sessions(dir.path()).unwrap();
        assert!(restored_snapshot.lifecycle_leases.is_empty());
        assert_eq!(restored_snapshot.sessions.len(), 1);
        assert_eq!(restored_snapshot.sessions[0].id, "live");
        assert_eq!(
            restored_snapshot.incarnation_high_water,
            crate::daemon_protocol::SessionIncarnation(50)
        );
        let mut proto = state.protocol.write().await;
        assert!(proto.sessions.contains_key("live"));
        assert!(
            proto.lifecycle_leases.is_empty(),
            "restart must release abandoned pre-launch authority"
        );
        assert_eq!(
            proto.reserve_start("next").unwrap(),
            crate::daemon_protocol::StartDisposition::Reserved(
                crate::daemon_protocol::ResourceOwner {
                    session_id: "next".into(),
                    incarnation: crate::daemon_protocol::SessionIncarnation(51),
                }
            ),
            "the first post-restart token must exceed removed persisted owners"
        );
    }

    #[tokio::test]
    async fn restore_never_deletes_incumbent_worktree_claimed_by_abandoned_restart() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let worktree = repo.join(".ouija/worktrees/restart");
        let data_dir = root.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let run_git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_git(&["init", "-b", "main", repo.to_str().unwrap()]);
        run_git(&[
            "-C",
            repo.to_str().unwrap(),
            "config",
            "user.email",
            "test@example.com",
        ]);
        run_git(&["-C", repo.to_str().unwrap(), "config", "user.name", "Test"]);
        std::fs::write(repo.join("tracked"), "incumbent\n").unwrap();
        run_git(&["-C", repo.to_str().unwrap(), "add", "tracked"]);
        run_git(&["-C", repo.to_str().unwrap(), "commit", "-m", "initial"]);
        run_git(&[
            "-C",
            repo.to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "restart",
            worktree.to_str().unwrap(),
        ]);

        let incumbent = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(1),
        };
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![crate::persistence::PersistedSession {
                id: incumbent.session_id.clone(),
                pane: None,
                registered_at: chrono::Utc::now(),
                last_activity_at: chrono::Utc::now(),
                metadata: crate::state::SessionMetadata {
                    backend: Some("opencode".into()),
                    project_dir: Some(worktree.to_string_lossy().into_owned()),
                    session_incarnation: crate::daemon_protocol::SessionIncarnation(2),
                    ..Default::default()
                },
            }],
            std::collections::BTreeMap::new(),
            crate::daemon_protocol::SessionIncarnation(2),
            std::collections::BTreeMap::from([(
                incumbent.session_id.clone(),
                crate::daemon_protocol::LifecycleLease {
                    owner: incumbent.clone(),
                    phase: crate::daemon_protocol::LifecyclePhase::Restarting,
                    backend: None,
                    backend_session_id: None,
                    backend_session_owner: None,
                    restart_target_owner: None,
                    restart_previous: None,
                    project_dir: Some(worktree.to_string_lossy().into_owned()),
                    project_dir_owner: Some(incumbent),
                    project_dir_cleanup_on_abandon: false,
                    inert_pane: None,
                    inert_pane_owner: None,
                },
            )]),
        );
        crate::persistence::save_sessions(&data_dir, &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: data_dir.clone(),
            config_dir: data_dir.clone(),
        });

        restore_persisted_sessions(&state).await.unwrap();

        assert!(
            worktree.join("tracked").is_file(),
            "an abandoned restart may terminalize its staged row but must preserve the incumbent worktree"
        );
        let restored = crate::persistence::load_sessions(&data_dir).unwrap();
        assert!(restored.sessions.is_empty());
        assert!(restored.lifecycle_leases.is_empty());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn restore_cleans_owned_worktree_below_symlinked_managed_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let data_dir = root.path().join("data");
        let home = root.path().join("home");
        let physical_managed_root = root.path().join("managed-storage");
        let raw_managed_root = home.join(".ouija");
        let raw_worktree = raw_managed_root.join("worktrees/repo/worker");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(physical_managed_root.join("worktrees/repo")).unwrap();
        symlink(&physical_managed_root, &raw_managed_root).unwrap();

        let run_git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_git(&["init", "-b", "main", repo.to_str().unwrap()]);
        run_git(&[
            "-C",
            repo.to_str().unwrap(),
            "config",
            "user.email",
            "test@example.com",
        ]);
        run_git(&["-C", repo.to_str().unwrap(), "config", "user.name", "Test"]);
        std::fs::write(repo.join("tracked"), "base\n").unwrap();
        run_git(&["-C", repo.to_str().unwrap(), "add", "tracked"]);
        run_git(&["-C", repo.to_str().unwrap(), "commit", "-m", "initial"]);
        run_git(&[
            "-C",
            repo.to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "worker",
            raw_worktree.to_str().unwrap(),
        ]);

        let canonical_worktree = crate::state::project_dir_identity(raw_worktree.to_str().unwrap());
        assert!(
            !canonical_worktree.contains("/.ouija/worktrees/"),
            "the physical identity must exercise recovery without a managed-path marker"
        );
        let owner = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(1),
        };
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![],
            std::collections::BTreeMap::new(),
            owner.incarnation,
            std::collections::BTreeMap::from([(
                owner.session_id.clone(),
                crate::daemon_protocol::LifecycleLease {
                    owner: owner.clone(),
                    phase: crate::daemon_protocol::LifecyclePhase::Starting,
                    backend: None,
                    backend_session_id: None,
                    backend_session_owner: None,
                    restart_target_owner: None,
                    restart_previous: None,
                    project_dir: Some(canonical_worktree),
                    project_dir_owner: Some(owner),
                    project_dir_cleanup_on_abandon: true,
                    inert_pane: None,
                    inert_pane_owner: None,
                },
            )]),
        );
        crate::persistence::save_sessions(&data_dir, &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: data_dir.clone(),
            config_dir: data_dir.clone(),
        });

        restore_persisted_sessions(&state).await.unwrap();

        assert!(
            !raw_worktree.exists(),
            "cleanup authority must survive canonicalizing a symlinked managed root"
        );
        assert!(
            crate::persistence::load_sessions(&data_dir)
                .unwrap()
                .lifecycle_leases
                .is_empty()
        );
    }

    #[tokio::test]
    async fn restore_finishes_owned_stop_after_registry_removal() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let worktree = repo.join(".ouija/worktrees/worker");
        let data_dir = root.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let run_git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_git(&["init", "-b", "main", repo.to_str().unwrap()]);
        run_git(&[
            "-C",
            repo.to_str().unwrap(),
            "config",
            "user.email",
            "test@example.com",
        ]);
        run_git(&["-C", repo.to_str().unwrap(), "config", "user.name", "Test"]);
        std::fs::write(repo.join("tracked"), "base\n").unwrap();
        run_git(&["-C", repo.to_str().unwrap(), "add", "tracked"]);
        run_git(&["-C", repo.to_str().unwrap(), "commit", "-m", "initial"]);
        run_git(&[
            "-C",
            repo.to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "worker",
            worktree.to_str().unwrap(),
        ]);

        let owner = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(1),
        };
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![],
            std::collections::BTreeMap::new(),
            owner.incarnation,
            std::collections::BTreeMap::from([(
                owner.session_id.clone(),
                crate::daemon_protocol::LifecycleLease {
                    owner: owner.clone(),
                    phase: crate::daemon_protocol::LifecyclePhase::Stopping,
                    backend: None,
                    backend_session_id: None,
                    backend_session_owner: None,
                    restart_target_owner: None,
                    restart_previous: None,
                    project_dir: Some(crate::state::project_dir_identity(
                        worktree.to_str().unwrap(),
                    )),
                    project_dir_owner: Some(owner.clone()),
                    project_dir_cleanup_on_abandon: true,
                    inert_pane: Some("%999999999".into()),
                    inert_pane_owner: Some(owner.clone()),
                },
            )]),
        );
        crate::persistence::save_sessions(&data_dir, &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: data_dir.clone(),
            config_dir: data_dir.clone(),
        });

        restore_persisted_sessions(&state).await.unwrap();

        assert!(
            !worktree.exists(),
            "startup recovery must finish the explicit kill's durable worktree cleanup"
        );
        assert!(
            crate::persistence::load_sessions(&data_dir)
                .unwrap()
                .lifecycle_leases
                .is_empty()
        );
        assert!(matches!(
            state
                .protocol
                .write()
                .await
                .reserve_start("worker")
                .unwrap(),
            crate::daemon_protocol::StartDisposition::Reserved(_)
        ));
    }

    #[tokio::test]
    async fn restore_aborts_claimed_http_backend_before_releasing_stop_lease() {
        use axum::Router;
        use axum::extract::{Path, State};
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone)]
        struct AbortProbe {
            data_dir: std::path::PathBuf,
            calls: std::sync::Arc<AtomicUsize>,
        }

        async fn abort_backend(
            Path(backend_session_id): Path<String>,
            State(probe): State<AbortProbe>,
        ) -> StatusCode {
            assert_eq!(backend_session_id, "ses_worker");
            assert!(
                crate::persistence::load_sessions(&probe.data_dir)
                    .unwrap()
                    .lifecycle_leases
                    .contains_key("worker"),
                "the stopping lease must remain durable through HTTP abort"
            );
            probe.calls.fetch_add(1, Ordering::SeqCst);
            StatusCode::NO_CONTENT
        }

        for pane in [Some("%999999999"), None] {
            let dir = tempfile::tempdir().unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let abort_port = listener.local_addr().unwrap().port();
            let daemon_port = abort_port.checked_sub(320).unwrap();
            let calls = std::sync::Arc::new(AtomicUsize::new(0));
            let app = Router::new()
                .route("/session/{session_id}/abort", post(abort_backend))
                .with_state(AbortProbe {
                    data_dir: dir.path().to_path_buf(),
                    calls: calls.clone(),
                });
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let owner = crate::daemon_protocol::ResourceOwner {
                session_id: "worker".into(),
                incarnation: crate::daemon_protocol::SessionIncarnation(7),
            };
            let snapshot = crate::persistence::PersistedLifecycleState::new(
                vec![],
                std::collections::BTreeMap::new(),
                owner.incarnation,
                std::collections::BTreeMap::from([(
                    owner.session_id.clone(),
                    crate::daemon_protocol::LifecycleLease {
                        owner: owner.clone(),
                        phase: crate::daemon_protocol::LifecyclePhase::Stopping,
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_worker".into()),
                        backend_session_owner: Some(owner.clone()),
                        restart_target_owner: None,
                        restart_previous: None,
                        project_dir: None,
                        project_dir_owner: None,
                        project_dir_cleanup_on_abandon: false,
                        inert_pane: pane.map(str::to_owned),
                        inert_pane_owner: pane.map(|_| owner.clone()),
                    },
                )]),
            );
            crate::persistence::save_sessions(dir.path(), &snapshot).unwrap();
            let state = crate::state::AppState::new(crate::config::OuijaConfig {
                name: "test".into(),
                npub: "npub1test".into(),
                port: daemon_port,
                data_dir: dir.path().to_path_buf(),
                config_dir: dir.path().to_path_buf(),
            });

            restore_persisted_sessions(&state).await.unwrap();

            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(
                crate::persistence::load_sessions(dir.path())
                    .unwrap()
                    .lifecycle_leases
                    .is_empty()
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn restore_never_aborts_replacement_using_same_http_backend_session() {
        let dir = tempfile::tempdir().unwrap();
        let stale_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(7),
        };
        let replacement_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(8),
        };
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![crate::persistence::PersistedSession {
                id: "worker".into(),
                pane: None,
                registered_at: chrono::Utc::now(),
                last_activity_at: chrono::Utc::now(),
                metadata: crate::state::SessionMetadata {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_worker".into()),
                    session_incarnation: replacement_owner.incarnation,
                    ..Default::default()
                },
            }],
            std::collections::BTreeMap::new(),
            replacement_owner.incarnation,
            std::collections::BTreeMap::from([(
                stale_owner.session_id.clone(),
                crate::daemon_protocol::LifecycleLease {
                    owner: stale_owner.clone(),
                    phase: crate::daemon_protocol::LifecyclePhase::Stopping,
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_worker".into()),
                    backend_session_owner: Some(stale_owner),
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
        crate::persistence::save_sessions(dir.path(), &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });

        restore_persisted_sessions(&state).await.unwrap();

        let persisted = crate::persistence::load_sessions(dir.path()).unwrap();
        assert!(persisted.lifecycle_leases.is_empty());
        assert_eq!(persisted.sessions.len(), 1);
        assert_eq!(
            persisted.sessions[0].metadata.session_incarnation,
            replacement_owner.incarnation
        );
        assert_eq!(
            state.protocol.read().await.sessions["worker"].owner(),
            replacement_owner
        );
    }

    #[tokio::test]
    async fn restore_retains_stop_lease_when_http_abort_is_not_confirmed() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;

        async fn reject_abort() -> StatusCode {
            StatusCode::INTERNAL_SERVER_ERROR
        }

        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let abort_port = listener.local_addr().unwrap().port();
        let daemon_port = abort_port.checked_sub(320).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/session/{session_id}/abort", post(reject_abort)),
            )
            .await
            .unwrap();
        });
        let owner = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(9),
        };
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![],
            std::collections::BTreeMap::new(),
            owner.incarnation,
            std::collections::BTreeMap::from([(
                owner.session_id.clone(),
                crate::daemon_protocol::LifecycleLease {
                    owner: owner.clone(),
                    phase: crate::daemon_protocol::LifecyclePhase::Stopping,
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_worker".into()),
                    backend_session_owner: Some(owner),
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
        crate::persistence::save_sessions(dir.path(), &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: daemon_port,
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });

        let result = restore_persisted_sessions(&state).await;

        assert!(result.is_err());
        assert!(
            crate::persistence::load_sessions(dir.path())
                .unwrap()
                .lifecycle_leases
                .contains_key("worker")
        );
        server.abort();
    }

    #[tokio::test]
    async fn queued_replacement_reselects_and_recovers_worktree_after_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let legacy = repo.join(".ouija/worktrees/worker");
        let replacement = root.path().join("home/.ouija/worktrees/repo/worker");
        let data_dir = root.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let run_git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_git(&["init", "-b", "main", repo.to_str().unwrap()]);
        run_git(&[
            "-C",
            repo.to_str().unwrap(),
            "config",
            "user.email",
            "test@example.com",
        ]);
        run_git(&["-C", repo.to_str().unwrap(), "config", "user.name", "Test"]);
        std::fs::write(repo.join("tracked"), "base\n").unwrap();
        run_git(&["-C", repo.to_str().unwrap(), "add", "tracked"]);
        run_git(&["-C", repo.to_str().unwrap(), "commit", "-m", "initial"]);
        run_git(&[
            "-C",
            repo.to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "worker",
            legacy.to_str().unwrap(),
        ]);

        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: data_dir.clone(),
            config_dir: data_dir.clone(),
        });
        let owner = match state.reserve_start("worker").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            disposition => panic!("expected reservation, got {disposition:?}"),
        };
        let stale_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "stale".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(0),
        };
        let cleanup_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let cleanup_removed = std::sync::Arc::new(tokio::sync::Notify::new());
        let release_cleanup = std::sync::Arc::new(tokio::sync::Notify::new());
        let cleanup_state = state.clone();
        let cleanup_legacy = legacy.to_string_lossy().into_owned();
        let started = cleanup_started.clone();
        let removed = cleanup_removed.clone();
        let release = release_cleanup.clone();
        let cleanup_action_dir = cleanup_legacy.clone();
        let cleanup = tokio::spawn(async move {
            cleanup_state
                .with_owned_worktree_cleanup(&stale_owner, &cleanup_legacy, || async move {
                    started.notify_one();
                    crate::state::AppState::cleanup_worktree_dir(&cleanup_action_dir).await;
                    removed.notify_one();
                    release.notified().await;
                })
                .await
        });
        cleanup_started.notified().await;

        let claim_state = state.clone();
        let claim_owner = owner.clone();
        let candidates = vec![
            legacy.to_string_lossy().into_owned(),
            replacement.to_string_lossy().into_owned(),
        ];
        let select_legacy = candidates[0].clone();
        let select_replacement = candidates[1].clone();
        let claim_repo = repo.clone();
        let claim = tokio::spawn(async move {
            claim_state
                .with_reserved_project_dir_choice(
                    &claim_owner,
                    claim_owner.clone(),
                    &candidates,
                    move || {
                        if std::path::Path::new(&select_legacy).exists() {
                            select_legacy
                        } else {
                            select_replacement
                        }
                    },
                    move |selected| async move {
                        let parent = std::path::Path::new(&selected).parent().unwrap();
                        std::fs::create_dir_all(parent).unwrap();
                        let output = std::process::Command::new("git")
                            .args([
                                "-C",
                                claim_repo.to_str().unwrap(),
                                "worktree",
                                "add",
                                &selected,
                                "worker",
                            ])
                            .output()
                            .unwrap();
                        assert!(
                            output.status.success(),
                            "replacement worktree creation failed: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                        selected
                    },
                )
                .await
        });
        cleanup_removed.notified().await;
        assert!(
            !claim.is_finished(),
            "replacement selection must wait for cleanup's directory gate"
        );
        release_cleanup.notify_one();
        assert_eq!(cleanup.await.unwrap(), Some(()));
        let selected = claim.await.unwrap().unwrap().unwrap();
        assert_eq!(selected, replacement.to_string_lossy());
        let persisted = crate::persistence::load_sessions(&data_dir).unwrap();
        assert!(
            persisted.lifecycle_leases["worker"].project_dir_cleanup_on_abandon,
            "replacement created after queued cleanup must gain recovery cleanup authority"
        );

        drop(state);
        let recovery_state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: data_dir.clone(),
            config_dir: data_dir.clone(),
        });
        restore_persisted_sessions(&recovery_state).await.unwrap();

        assert!(
            !replacement.exists(),
            "startup recovery must remove the abandoned replacement worktree"
        );
        assert!(
            crate::persistence::load_sessions(&data_dir)
                .unwrap()
                .lifecycle_leases
                .is_empty()
        );
    }

    #[tokio::test]
    async fn restore_persisted_sessions_removes_unlaunched_http_start_before_releasing_id() {
        let dir = tempfile::tempdir().unwrap();
        let owner = crate::daemon_protocol::ResourceOwner {
            session_id: "pending".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(49),
        };
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![crate::persistence::PersistedSession {
                id: owner.session_id.clone(),
                pane: None,
                registered_at: chrono::Utc::now(),
                last_activity_at: chrono::Utc::now(),
                metadata: crate::state::SessionMetadata {
                    backend: Some("opencode".into()),
                    session_incarnation: owner.incarnation,
                    ..Default::default()
                },
            }],
            std::collections::BTreeMap::new(),
            owner.incarnation,
            std::collections::BTreeMap::from([(
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
                    inert_pane: None,
                    inert_pane_owner: None,
                },
            )]),
        );
        crate::persistence::save_sessions(dir.path(), &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });

        restore_persisted_sessions(&state).await.unwrap();

        let restored_snapshot = crate::persistence::load_sessions(dir.path()).unwrap();
        assert!(restored_snapshot.sessions.is_empty());
        assert!(restored_snapshot.lifecycle_leases.is_empty());
        let mut proto = state.protocol.write().await;
        assert!(!proto.sessions.contains_key("pending"));
        assert!(matches!(
            proto.reserve_start("pending").unwrap(),
            crate::daemon_protocol::StartDisposition::Reserved(_)
        ));
    }

    #[tokio::test]
    async fn restore_persisted_sessions_distinguishes_unstaged_and_staged_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let incumbent_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "incumbent".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(40),
        };
        let staged_lease_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "staged".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(41),
        };
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            vec![
                crate::persistence::PersistedSession {
                    id: incumbent_owner.session_id.clone(),
                    pane: None,
                    registered_at: chrono::Utc::now(),
                    last_activity_at: chrono::Utc::now(),
                    metadata: crate::state::SessionMetadata {
                        backend: Some("opencode".into()),
                        session_incarnation: incumbent_owner.incarnation,
                        ..Default::default()
                    },
                },
                crate::persistence::PersistedSession {
                    id: staged_lease_owner.session_id.clone(),
                    pane: None,
                    registered_at: chrono::Utc::now(),
                    last_activity_at: chrono::Utc::now(),
                    metadata: crate::state::SessionMetadata {
                        backend: Some("opencode".into()),
                        session_incarnation: crate::daemon_protocol::SessionIncarnation(42),
                        ..Default::default()
                    },
                },
            ],
            std::collections::BTreeMap::new(),
            crate::daemon_protocol::SessionIncarnation(42),
            std::collections::BTreeMap::from([
                (
                    incumbent_owner.session_id.clone(),
                    crate::daemon_protocol::LifecycleLease {
                        owner: incumbent_owner.clone(),
                        phase: crate::daemon_protocol::LifecyclePhase::Restarting,
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
                ),
                (
                    staged_lease_owner.session_id.clone(),
                    crate::daemon_protocol::LifecycleLease {
                        owner: staged_lease_owner,
                        phase: crate::daemon_protocol::LifecyclePhase::Restarting,
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
                ),
            ]),
        );
        crate::persistence::save_sessions(dir.path(), &snapshot).unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });

        restore_persisted_sessions(&state).await.unwrap();

        let restored_snapshot = crate::persistence::load_sessions(dir.path()).unwrap();
        assert_eq!(restored_snapshot.sessions.len(), 1);
        assert_eq!(restored_snapshot.sessions[0].id, "incumbent");
        let proto = state.protocol.read().await;
        assert!(proto.sessions.contains_key("incumbent"));
        assert!(!proto.sessions.contains_key("staged"));
        assert!(proto.lifecycle_leases.is_empty());
    }

    #[tokio::test]
    async fn restore_persisted_sessions_rolls_pending_restart_back_to_literal_incumbent() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::delete;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        async fn delete_session(AxumState(calls): AxumState<Arc<AtomicUsize>>) -> StatusCode {
            calls.fetch_add(1, Ordering::SeqCst);
            StatusCode::NO_CONTENT
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}", delete(delete_session))
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
        let state = crate::state::AppState::new(config.clone());
        let incumbent_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "pending-restart".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(40),
        };
        {
            let mut proto = state.protocol.write().await;
            proto.restore_incarnation_high_water(incumbent_owner.incarnation);
            proto.sessions.insert(
                incumbent_owner.session_id.clone(),
                crate::daemon_protocol::SessionEntry {
                    id: incumbent_owner.session_id.clone(),
                    pane: None,
                    origin: crate::daemon_protocol::Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_incumbent".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        model: Some("incumbent-model".into()),
                        fresh_context_after_active_secs: Some(60),
                        active_context_accumulated_secs: 61,
                        active_context_segment_started_at: Some(100),
                        active_context_restart_due: true,
                        last_metadata_update: Some(777),
                        session_incarnation: incumbent_owner.incarnation,
                        ..Default::default()
                    },
                    registered_at: 123,
                    active_context_due_boundary: Default::default(),
                },
            );
            state.persist_protocol_state(&proto).unwrap();
        }
        assert_eq!(
            state.claim_existing_start(&incumbent_owner).await.unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        let target_incarnation = match state
            .stage_restart_launch(
                &incumbent_owner,
                "opencode".into(),
                true,
                true,
                Some(120),
                None,
                None,
            )
            .await
        {
            crate::daemon_protocol::StageFreshLaunchOutcome::Staged { incarnation } => incarnation,
            outcome => panic!("expected staged restart target, got {outcome:?}"),
        };
        assert_ne!(target_incarnation, incumbent_owner.incarnation);
        {
            let protocol = state.protocol.read().await;
            let staged = &protocol.sessions["pending-restart"].metadata;
            assert_eq!(staged.fresh_context_after_active_secs, Some(120));
            assert_eq!(staged.active_context_accumulated_secs, 0);
            assert_eq!(staged.active_context_segment_started_at, None);
            assert!(!staged.active_context_restart_due);
            assert!(staged.active_context_accounting_provisional);
        }
        let target_owner = crate::daemon_protocol::ResourceOwner {
            session_id: incumbent_owner.session_id.clone(),
            incarnation: target_incarnation,
        };
        assert_eq!(
            state
                .record_restart_backend_claim(
                    &incumbent_owner,
                    &target_owner,
                    "opencode".into(),
                    "ses_target".into(),
                )
                .await
                .unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );

        let recovery = crate::state::AppState::new(config);
        restore_persisted_sessions(&recovery).await.unwrap();

        let restored_snapshot = crate::persistence::load_sessions(dir.path()).unwrap();
        assert!(restored_snapshot.lifecycle_leases.is_empty());
        assert_eq!(restored_snapshot.sessions.len(), 1);
        let restored = &restored_snapshot.sessions[0];
        assert_eq!(restored.id, incumbent_owner.session_id);
        assert_eq!(
            restored.metadata.session_incarnation,
            incumbent_owner.incarnation
        );
        assert_eq!(
            restored.metadata.backend_session_id.as_deref(),
            Some("ses_incumbent")
        );
        assert_eq!(restored.metadata.model.as_deref(), Some("incumbent-model"));
        assert_eq!(restored.metadata.fresh_context_after_active_secs, Some(60));
        assert_eq!(restored.metadata.active_context_accumulated_secs, 61);
        assert_eq!(
            restored.metadata.active_context_segment_started_at,
            Some(100)
        );
        assert!(restored.metadata.active_context_restart_due);
        assert!(!restored.metadata.active_context_accounting_provisional);
        assert_eq!(
            restored
                .metadata
                .last_metadata_update
                .map(|timestamp| timestamp.timestamp()),
            Some(777)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[test]
    fn restart_reconciliation_removes_same_incarnation_inert_fallback_row() {
        let owner = crate::daemon_protocol::ResourceOwner {
            session_id: "resumed".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(43),
        };
        let session = crate::persistence::PersistedSession {
            id: owner.session_id.clone(),
            pane: Some("%fallback".into()),
            registered_at: chrono::Utc::now(),
            last_activity_at: chrono::Utc::now(),
            metadata: crate::state::SessionMetadata {
                session_incarnation: owner.incarnation,
                ..Default::default()
            },
        };
        let lease = crate::daemon_protocol::LifecycleLease {
            owner: owner.clone(),
            phase: crate::daemon_protocol::LifecyclePhase::Restarting,
            backend: None,
            backend_session_id: None,
            backend_session_owner: None,
            restart_target_owner: None,
            restart_previous: None,
            project_dir: None,
            project_dir_owner: None,
            project_dir_cleanup_on_abandon: false,
            inert_pane: Some("%fallback".into()),
            inert_pane_owner: Some(owner),
        };

        assert!(abandoned_lease_owns_staged_row(&session, &lease));
    }

    #[test]
    fn direct_respawn_recovery_accepts_incumbent_and_staged_pane_owners() {
        let incumbent = crate::daemon_protocol::ResourceOwner {
            session_id: "resumed".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(43),
        };
        let staged = crate::daemon_protocol::ResourceOwner {
            session_id: "resumed".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(44),
        };
        let lease = crate::daemon_protocol::LifecycleLease {
            owner: incumbent.clone(),
            phase: crate::daemon_protocol::LifecyclePhase::Restarting,
            backend: None,
            backend_session_id: None,
            backend_session_owner: None,
            restart_target_owner: None,
            restart_previous: None,
            project_dir: None,
            project_dir_owner: None,
            project_dir_cleanup_on_abandon: false,
            inert_pane: Some("%existing".into()),
            inert_pane_owner: Some(staged.clone()),
        };

        assert_eq!(lifecycle_lease_pane_owners(&lease), vec![incumbent, staged]);
    }

    #[tokio::test]
    async fn restore_persisted_sessions_fails_closed_on_corrupt_authority() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sessions.json"), "{not-json").unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: 0,
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });

        let result = restore_persisted_sessions(&state).await;

        assert!(
            result.is_err(),
            "daemon startup must not enable lifecycle allocation after authority load failure"
        );
    }

    #[test]
    fn stop_plan_stops_systemd_and_legacy_when_unit_exists() {
        let plan = plan_daemon_stop(true);
        assert!(plan.stop_systemd);
        assert!(
            plan.stop_legacy,
            "stop-server must still clean up stray legacy daemons when a unit exists"
        );
    }

    #[test]
    fn stop_plan_uses_legacy_only_without_systemd_unit() {
        let plan = plan_daemon_stop(false);
        assert!(!plan.stop_systemd);
        assert!(plan.stop_legacy);
    }

    #[test]
    fn supervised_lifecycle_uses_systemd_for_active_unit() {
        assert_eq!(
            plan_supervised_lifecycle(true, true),
            DaemonLifecyclePlan::SystemdOnly
        );
    }

    #[test]
    fn supervised_lifecycle_cleans_legacy_before_inactive_unit_start() {
        assert_eq!(
            plan_supervised_lifecycle(true, false),
            DaemonLifecyclePlan::SystemdAfterLegacyCleanup
        );
    }

    #[test]
    fn supervised_lifecycle_uses_legacy_without_systemd_unit() {
        assert_eq!(
            plan_supervised_lifecycle(false, false),
            DaemonLifecyclePlan::LegacyOnly
        );
    }

    #[test]
    fn stop_daemon_processes_runs_legacy_cleanup_when_systemd_stop_fails() {
        use std::cell::Cell;

        let legacy_called = Cell::new(false);
        let err = stop_daemon_processes_with(
            true,
            true,
            || -> anyhow::Result<()> { anyhow::bail!("systemd stop failed") },
            || {
                legacy_called.set(true);
                LegacyStopOutcome {
                    tmux_killed: true,
                    process_killed: false,
                }
            },
        )
        .unwrap_err();

        assert!(legacy_called.get());
        assert!(err.to_string().contains("systemd stop failed"));
    }

    #[test]
    fn systemd_after_legacy_cleanup_has_settle_delay() {
        assert_eq!(
            legacy_cleanup_settle_delay(&DaemonLifecyclePlan::SystemdAfterLegacyCleanup),
            Some(std::time::Duration::from_secs(1))
        );
        assert_eq!(
            legacy_cleanup_settle_delay(&DaemonLifecyclePlan::SystemdOnly),
            None
        );
        assert_eq!(
            legacy_cleanup_settle_delay(&DaemonLifecyclePlan::LegacyOnly),
            None
        );
    }

    // --- whoami identity diagnostics (task #1395) ---
    //
    // An opencode agent whose bash runs outside tmux guessed its sender id
    // from the project basename, impersonating a sibling session and
    // misrouting the reply. `ouija whoami` must resolve through the exact
    // same signal path as `require_my_session_id`, report WHICH signal won,
    // and on failure explain what was missing without ever inviting a guess.

    #[test]
    fn pick_session_id_reports_pane_var_as_source() {
        let res = pick_session_id(Some("%3"), Some("keycast".into()), None);
        assert_eq!(
            res,
            SessionIdResolution::Found("keycast".into(), IdentitySource::PaneVar)
        );
    }

    #[test]
    fn pick_session_id_reports_env_var_as_source() {
        let res = pick_session_id(None, None, Some("hub".into()));
        assert_eq!(
            res,
            SessionIdResolution::Found("hub".into(), IdentitySource::EnvVar)
        );
    }

    #[test]
    fn whoami_failure_outside_tmux_lists_missing_signals_and_forbids_guessing() {
        let failure = WhoamiFailure {
            tmux_pane: None,
            pane_var: None,
            env_var: None,
            lookup: None,
        };
        let msg = format_whoami_failure(&failure);
        assert!(
            msg.contains("$TMUX_PANE: not set"),
            "must report the missing tmux pane signal, got: {msg}"
        );
        assert!(
            msg.contains("$OUIJA_SESSION_ID: not set"),
            "must report the missing env var signal, got: {msg}"
        );
        assert!(
            msg.contains("Never guess"),
            "must explicitly forbid guessing a sender id, got: {msg}"
        );
        assert!(
            msg.contains("project directory"),
            "must call out the project-basename guess that caused the incident, got: {msg}"
        );
        assert!(
            !msg.contains("ouija register"),
            "must never steer an unresolved caller toward `ouija register`, got: {msg}"
        );
    }

    #[test]
    fn whoami_failure_in_tmux_reports_pane_lookup_miss() {
        let failure = WhoamiFailure {
            tmux_pane: Some("%3".into()),
            pane_var: None,
            env_var: None,
            lookup: Some(PaneLookupFailure::NoSessionForPane),
        };
        let msg = format_whoami_failure(&failure);
        assert!(
            msg.contains("$TMUX_PANE: %3"),
            "must show the pane that was checked, got: {msg}"
        );
        assert!(
            msg.contains("@ouija_session"),
            "must report the pane var signal by name, got: {msg}"
        );
        assert!(
            msg.contains("no registered session"),
            "must say the daemon lookup found nothing for this pane, got: {msg}"
        );
    }

    #[test]
    fn whoami_failure_reports_unreachable_daemon() {
        let failure = WhoamiFailure {
            tmux_pane: Some("%3".into()),
            pane_var: None,
            env_var: None,
            lookup: Some(PaneLookupFailure::DaemonUnreachable(
                "http://localhost:7880".into(),
            )),
        };
        let msg = format_whoami_failure(&failure);
        assert!(
            msg.contains("daemon unreachable at http://localhost:7880"),
            "must distinguish an unreachable daemon from a pane miss, got: {msg}"
        );
    }

    #[test]
    fn whoami_unregistered_identity_names_id_and_source_without_guessing() {
        let msg = format_unregistered_identity("stale-id", &IdentitySource::EnvVar);
        assert!(
            msg.contains("stale-id"),
            "must name the resolved-but-unregistered id, got: {msg}"
        );
        assert!(
            msg.contains("$OUIJA_SESSION_ID"),
            "must say which signal produced the stale id, got: {msg}"
        );
        assert!(
            msg.contains("renamed"),
            "must explain the likely cause (rename/removal), got: {msg}"
        );
        assert!(
            msg.contains("Never guess"),
            "must forbid guessing a replacement id, got: {msg}"
        );
        assert!(
            !msg.contains("ouija register"),
            "must never suggest `ouija register`, got: {msg}"
        );
    }

    #[test]
    fn status_lists_local_session_matches_local_origin_only() {
        let status = serde_json::json!({
            "sessions": [
                {"id": "mine", "origin": "local"},
                {"id": "peer/mine", "origin": "remote"},
            ]
        });
        assert!(status_lists_local_session(&status, "mine"));
        assert!(
            !status_lists_local_session(&status, "peer/mine"),
            "a remote session id is never the local caller's identity"
        );
        assert!(!status_lists_local_session(&status, "absent"));
    }

    #[test]
    fn unresolved_sender_error_points_at_whoami_not_register() {
        let msg = unresolved_sender_error();
        assert!(
            msg.contains("ouija whoami"),
            "unresolved identity must steer callers to whoami diagnostics, got: {msg}"
        );
        assert!(
            msg.contains("Never guess"),
            "must forbid guessing a sender id, got: {msg}"
        );
        assert!(
            !msg.contains("ouija register"),
            "must never steer callers toward `ouija register`, got: {msg}"
        );
    }

    #[test]
    fn config_help_describes_default_backend_setting() {
        let mut command = Cli::command();
        let config = command
            .find_subcommand_mut("config")
            .expect("config subcommand");
        let mut help = Vec::new();
        config.write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        assert!(help.contains("default_backend"));
        assert!(help.contains("claude-code, opencode, codex-cli"));
    }
}
