mod admin;
mod api;
mod backend;
mod config;
pub mod daemon_protocol;
mod hooks;
mod nostr_transport;
mod persistence;
mod project_index;
mod protocol;
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
    Rename { new_id: String },
    /// Unregister a session (without killing it)
    Unregister { id: String },
    /// Start a new session
    #[command(name = "spawn-session")]
    SpawnSession {
        name: String,
        #[arg(long)]
        project_dir: Option<String>,
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
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        reminder: Option<String>,
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
enum ConfigAction {
    /// Set a config value (e.g. ouija config set auto_register false)
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
            restore_persisted_sessions(&state).await;
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
                    let panes_to_check: Vec<(String, String)> = {
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
                            .filter_map(|s| Some((s.id.clone(), s.pane.clone()?)))
                            .collect()
                    };
                    let dead_ids: Vec<String> = if !panes_to_check.is_empty() {
                        let names: Vec<String> = reaper_state.backends.all_process_names();
                        let dead = tokio::task::spawn_blocking(move || {
                            let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
                            panes_to_check
                                .into_iter()
                                .filter(|(_, pane)| !crate::tmux::pane_alive(pane, &name_refs))
                                .map(|(id, _)| id)
                                .collect::<Vec<_>>()
                        })
                        .await
                        .unwrap_or_default();
                        if !dead.is_empty() {
                            reaper_state
                                .apply_and_execute(crate::daemon_protocol::Event::ReapDead {
                                    dead_ids: dead.clone(),
                                })
                                .await;
                        }
                        dead
                    } else {
                        vec![]
                    };
                    // Clean up per-fire worktree panes
                    let perfire_to_check: Vec<(String, String)> = {
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
                            let mut pf = reaper_state.perfire_worktree_panes.write().await;
                            for (pane_id, project_dir) in dead_perfire {
                                pf.remove(&pane_id);
                                tracing::info!(
                                    "per-fire worktree pane {pane_id} died, pruning worktrees in {project_dir}"
                                );
                                let _ = tokio::task::spawn_blocking(move || {
                                    std::process::Command::new("git")
                                        .args(["-C", &project_dir, "worktree", "prune"])
                                        .status()
                                })
                                .await;
                            }
                        }
                    }
                    let _ = dead_ids; // suppress unused warning

                    // If over the max session limit, close the most idle ones.
                    // Killing the pane lets the next reaper cycle clean up + broadcast.
                    for id in reaper_state.collect_excess_idle_sessions().await {
                        tracing::info!(
                            "auto-closing idle session '{id}' (over max_local_sessions)"
                        );
                        crate::nostr_transport::kill_session(&reaper_state, &id).await;
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
        Command::Rename { new_id } => {
            let old_id = require_my_session_id().await?;
            let body = serde_json::json!({ "old_id": old_id, "new_id": new_id });
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
            prompt,
            reminder,
            model,
            effort,
        } => {
            let body = serde_json::json!({
                "name": name,
                "fresh": fresh,
                "prompt": prompt,
                "reminder": reminder,
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
                            "{:<10} {:<16} {:<16} {:<10} {:<10} {:<12} {:<8} {:<20} RUNS",
                            "ID",
                            "NAME",
                            "CRON",
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
                            let target = t["target_session"].as_str().unwrap_or("—");
                            let backend = t["backend"].as_str().unwrap_or("—");
                            let model = t["model"].as_str().unwrap_or("—");
                            let enabled = t["enabled"].as_bool().unwrap_or(false);
                            let next = t["next_run"].as_str().unwrap_or("-");
                            let runs = t["run_count"].as_u64().unwrap_or(0);
                            println!(
                                "{:<10} {:<16} {:<16} {:<10} {:<10} {:<12} {:<8} {:<20} {}",
                                id, name, cron, target, backend, model, enabled, next, runs
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

async fn restore_persisted_sessions(state: &state::AppState) {
    let sessions = match persistence::load_sessions(&state.config.data_dir) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to load persisted sessions: {e}");
            return;
        }
    };

    if sessions.is_empty() {
        return;
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
        return;
    }

    let mut proto = state.protocol.write().await;
    for ps in &alive {
        let entry = crate::daemon_protocol::SessionEntry {
            id: ps.id.clone(),
            pane: ps.pane.clone(),
            origin: crate::daemon_protocol::Origin::Local,
            metadata: metadata_for_restored_session(&ps.metadata),
            ..Default::default()
        };
        proto.sessions.insert(ps.id.clone(), entry);
    }
    tracing::info!("restored {} persisted sessions", alive.len());
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
) -> serde_json::Value {
    serde_json::json!({
        "pane": tmux_pane,
        "self_id": self_id,
        "backend_identity": backend_identity,
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

fn enforce_explicit_sender_match(explicit: &str, canonical: &str) -> anyhow::Result<String> {
    if explicit == canonical {
        Ok(explicit.into())
    } else {
        anyhow::bail!(
            "--from '{explicit}' does not match this backend identity's canonical session '{canonical}'; never stamp a raw or sibling sender id"
        )
    }
}

struct ResolvedSender {
    id: String,
    context: serde_json::Value,
}

/// Resolve a message sender and the proof sent alongside it from one identity
/// arbitration result. Keeping these together prevents `from` from naming the
/// backend-canonical session while `sender_ctx.self_id` still reports a stale
/// pane or environment value.
async fn resolve_sender(explicit: Option<String>) -> anyhow::Result<ResolvedSender> {
    match whoami_outcome().await {
        WhoamiOutcome::Resolved {
            id,
            source,
            tmux_pane,
            backend_identity,
        } => {
            verify_resolved_id_registered(&id, &source).await?;
            let id = match explicit {
                Some(explicit) => enforce_explicit_sender_match(&explicit, &id)?,
                None => id,
            };
            let context = sender_context(Some(&id), tmux_pane, backend_identity);
            Ok(ResolvedSender { id, context })
        }
        WhoamiOutcome::Conflict(conflict) => anyhow::bail!(format_identity_conflict(&conflict)),
        WhoamiOutcome::BackendResolutionFailed(error) => anyhow::bail!(
            "backend identity was discovered but could not be resolved safely: {error}"
        ),
        WhoamiOutcome::Unresolved(_) => {
            let Some(explicit) = explicit else {
                return Err(anyhow::anyhow!(unresolved_sender_error()));
            };
            let tmux_pane = std::env::var("TMUX_PANE")
                .ok()
                .filter(|pane| !pane.is_empty());
            let backend_identity =
                backend::BackendRegistry::default_registry().caller_session_identity();
            if let Some(identity) = backend_identity.as_ref() {
                let canonical = resolve_backend_identity_from_daemon(identity).await?;
                let id = enforce_explicit_sender_match(&explicit, &canonical)?;
                let context = sender_context(Some(&id), tmux_pane, backend_identity);
                Ok(ResolvedSender { id, context })
            } else {
                // Preserve explicit legacy sends that have no observable local
                // identity. There is no raw signal to report as `self_id`.
                let context = sender_context(None, tmux_pane, None);
                Ok(ResolvedSender {
                    id: explicit,
                    context,
                })
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn explicit_sender_must_match_canonical_backend_identity() {
        assert_eq!(
            enforce_explicit_sender_match("canonical", "canonical").unwrap(),
            "canonical"
        );
        let err = enforce_explicit_sender_match("sibling", "canonical").unwrap_err();
        assert!(err.to_string().contains("does not match"));
        assert!(err.to_string().contains("sibling"));
        assert!(err.to_string().contains("canonical"));
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
                "project_dir": "/home/daniel/code/ouija",
                "role": "working on ouija",
                "bulletin": "ready",
                "stale": true,
                "worktree_present": true,
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
                    "origin": "remote:locota"
                }]
            })
        );
    }

    #[test]
    fn metadata_for_restored_session_preserves_persisted_fields() {
        let metadata = crate::state::SessionMetadata {
            project_dir: Some("/tmp/project".into()),
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
            session_incarnation: 13,
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
}
