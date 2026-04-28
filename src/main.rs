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

use anyhow::Context;
use backend::CodingAssistant;
use clap::{Parser, Subcommand};
use nostr_sdk::ToBech32;

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
        message: String,
        /// Sender session ID. Required outside tmux when `$OUIJA_SESSION_ID` is unset.
        #[arg(long)]
        from: Option<String>,
    },
    /// Send a message (fire-and-forget)
    Tell {
        to: String,
        message: String,
        /// Thread as progress update for a pending reply
        #[arg(long)]
        reply_to: Option<u64>,
        /// Sender session ID. Required outside tmux when `$OUIJA_SESSION_ID` is unset.
        #[arg(long)]
        from: Option<String>,
    },
    /// Reply to a message (defaults to done=true)
    Reply {
        to: String,
        msg_id: u64,
        message: String,
        /// Don't mark as done (progress update)
        #[arg(long)]
        no_done: bool,
        /// Expect a reply back
        #[arg(long)]
        expect_reply: bool,
        /// Sender session ID. Required outside tmux when `$OUIJA_SESSION_ID` is unset.
        #[arg(long)]
        from: Option<String>,
    },
    /// List sessions
    Ls,
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
        #[arg(long)]
        reminder: Option<String>,
        #[arg(long)]
        worktree: bool,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        base_branch: Option<String>,
        /// LLM model (claude: alias/full id; opencode: providerID/modelID).
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort / variant (claude: --effort; opencode: prompt variant).
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
    ClearReminder { clearing_id: u64 },
    /// Clear a pending reply from a disconnected sender
    #[command(name = "clear-reply")]
    ClearReply { sender_id: String },
    /// Stop the running daemon
    #[command(name = "stop-server")]
    StopServer,
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
                        "error: no coding backend found in PATH. Install claude-code or opencode.\n\
                         See: https://docs.anthropic.com/en/docs/claude-code\n\
                         See: https://opencode.ai"
                    );
                    std::process::exit(1);
                }
                tracing::info!("available backends: {}", available.join(", "));
            }

            let config = config::OuijaConfig::new(name, port, data, npub)?;
            let state = state::AppState::new(config);

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
        Command::Ask { to, message, from } => {
            let from = match from {
                Some(id) => id,
                None => require_my_session_id().await?,
            };
            let body = serde_json::json!({
                "from": from,
                "to": to,
                "message": message,
                "expects_reply": true,
            });
            cli_post("/api/send", &body).await?;
        }
        Command::Tell {
            to,
            message,
            reply_to,
            from,
        } => {
            let from = match from {
                Some(id) => id,
                None => require_my_session_id().await?,
            };
            let body = serde_json::json!({
                "from": from,
                "to": to,
                "message": message,
                "expects_reply": false,
                "responds_to": reply_to,
            });
            cli_post("/api/send", &body).await?;
        }
        Command::Reply {
            to,
            msg_id,
            message,
            no_done,
            expect_reply,
            from,
        } => {
            let from = match from {
                Some(id) => id,
                None => require_my_session_id().await?,
            };
            let body = serde_json::json!({
                "from": from,
                "to": to,
                "message": message,
                "expects_reply": expect_reply,
                "responds_to": msg_id,
                "done": !no_done,
            });
            cli_post("/api/send", &body).await?;
        }
        Command::Ls => {
            cli_get("/api/status").await?;
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
            worktree,
            branch,
            base_branch,
            model,
            effort,
            backend,
            from,
        } => {
            let body = serde_json::json!({
                "name": name,
                "project_dir": project_dir,
                "prompt": prompt,
                "reminder": reminder,
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
            let dry_run = value.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(true);
            if dry_run {
                if let Some(arr) = value["would_prune"].as_array() {
                    let ids = arr.len();
                    if ids == 0 {
                        println!("No stale sessions to prune");
                    } else {
                        println!("Would prune {} stale session(s): {}",
                            ids, value["would_prune"]);
                        println!("Run with --yes to confirm removal");
                    }
                } else {
                    println!("No stale sessions to prune");
                }
            } else {
                if let Some(arr) = value["pruned"].as_array() {
                    println!("Pruned {} stale session(s)", arr.len());
                } else {
                    println!("Pruned 0 stale session(s)");
                }
                if let Some(arr) = value["errors"].as_array() {
                    eprintln!("Failed to prune {} session(s): {}",
                        arr.len(), value["errors"]);
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
        Command::ClearReminder { clearing_id } => {
            let from = require_my_session_id().await?;
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
                            "{:<10} {:<16} {:<16} {:<10} {:<8} {:<20} RUNS",
                            "ID", "NAME", "CRON", "TARGET", "ENABLED", "NEXT RUN"
                        );
                        for t in list {
                            let id = t["id"].as_str().unwrap_or("-");
                            let name = t["name"].as_str().unwrap_or("-");
                            let cron = t["cron"].as_str().unwrap_or("-");
                            let target = t["target_session"].as_str().unwrap_or("—");
                            let enabled = t["enabled"].as_bool().unwrap_or(false);
                            let next = t["next_run"].as_str().unwrap_or("-");
                            let runs = t["run_count"].as_u64().unwrap_or(0);
                            println!(
                                "{:<10} {:<16} {:<16} {:<10} {:<8} {:<20} {}",
                                id, name, cron, target, enabled, next, runs
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
                once,
            } => {
                let body = serde_json::json!({
                    "name": name,
                    "cron": cron,
                    "target_session": target,
                    "message": message,
                    "project_dir": project_dir,
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

    // Check pane liveness on blocking thread
    let names: Vec<String> = state.backends.all_process_names();
    let alive = tokio::task::spawn_blocking(move || {
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        sessions
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

    if alive.is_empty() {
        return;
    }

    let mut proto = state.protocol.write().await;
    for ps in &alive {
        let entry = crate::daemon_protocol::SessionEntry {
            id: ps.id.clone(),
            pane: ps.pane.clone(),
            origin: crate::daemon_protocol::Origin::Local,
            metadata: crate::daemon_protocol::SessionMeta {
                project_dir: ps.metadata.project_dir.clone(),
                role: ps.metadata.role.clone(),
                bulletin: ps.metadata.bulletin.clone(),
                networked: ps.metadata.networked,
                worktree: ps.metadata.worktree,
                vim_mode: ps.metadata.vim_mode,
                backend_session_id: ps.metadata.backend_session_id.clone(),
                backend: ps.metadata.backend.clone(),
                project_description: ps.metadata.project_description.clone(),
                last_metadata_update: ps.metadata.last_metadata_update.map(|dt| dt.timestamp()),
                model: ps.metadata.model.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        proto.sessions.insert(ps.id.clone(), entry);
    }
    tracing::info!("restored {} persisted sessions", alive.len());
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

fn stop_daemon() -> anyhow::Result<()> {
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

    if tmux_killed || pkill_killed {
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
    let daemon_alive = Cmd::new("curl")
        .args(["-sf", &status_url])
        .stdout(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if latest == current {
        println!("already on latest version ({current})");
        backend::claude_code::refresh_plugin_cache(&latest);
        if !daemon_alive {
            println!("daemon is not running — starting it...");
            Cmd::new("ouija")
                .arg("start-server")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("failed to spawn ouija start-server")?;
            for i in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if Cmd::new("curl")
                    .args(["-sf", &status_url])
                    .stdout(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
                {
                    break;
                }
                if i == 19 {
                    eprintln!("warning: daemon did not start within 10s");
                }
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

    println!("restarting daemon...");
    stop_daemon()?;
    std::thread::sleep(std::time::Duration::from_secs(1));

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

    Cmd::new("ouija")
        .arg("start-server")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn ouija start-server")?;

    let status_url = format!("http://localhost:{port}/api/status");
    for i in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if Cmd::new("curl")
            .args(["-sf", &status_url])
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            println!("ouija updated to {latest} and running");
            println!("dashboard: http://localhost:{port}");
            return Ok(());
        }
        if i == 19 {
            anyhow::bail!("daemon did not start within 10s");
        }
    }
    Ok(())
}

/// Query crates.io for the latest version of a crate (including prereleases).
fn fetch_latest_crate_version(name: &str) -> anyhow::Result<String> {
    use std::process::Command as Cmd;

    let output = Cmd::new("curl")
        .args(["-sf", &format!("https://crates.io/api/v1/crates/{name}")])
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

/// Look up the registered session ID for the current execution context.
///
/// Resolution order:
/// 1. `$OUIJA_SESSION_ID` env var — explicit override for non-tmux engines
///    (e.g. opencode HTTP API) and plugin wrappers.
/// 2. `@ouija_session` tmux pane variable — fast path for tmux callers.
/// 3. `/api/status` lookup by `$TMUX_PANE` — fallback when the pane var was
///    cleared but the daemon still tracks the pane.
async fn resolve_my_session_id() -> Option<String> {
    if let Ok(id) = std::env::var("OUIJA_SESSION_ID") {
        if !id.is_empty() {
            return Some(id);
        }
    }

    let pane = std::env::var("TMUX_PANE").ok()?;

    // Fast path: tmux pane variable (no HTTP)
    if let Some(id) = tmux_var::get(&pane) {
        return Some(id);
    }

    // Fallback: query daemon API
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!("http://localhost:{port}/api/status");
    let resp = reqwest::get(&url).await.ok()?;
    let status: serde_json::Value = resp.json().await.ok()?;
    status["sessions"]
        .as_array()?
        .iter()
        .find(|s| s["pane"].as_str() == Some(&pane))
        .and_then(|s| s["id"].as_str().map(String::from))
}

/// Resolve session ID or bail with a helpful error.
///
/// The error message intentionally never instructs the caller to run
/// `ouija register`: in non-tmux engines (e.g. opencode HTTP API) an LLM
/// reading the error literally would self-trigger a ghost-shape register
/// call. Steer callers to `--from <id>` or `OUIJA_SESSION_ID` instead.
async fn require_my_session_id() -> anyhow::Result<String> {
    resolve_my_session_id().await.ok_or_else(|| {
        anyhow::anyhow!(
            "unable to resolve the current session ID. \
             Pass `--from <your-session-id>` to this command, \
             or export `OUIJA_SESSION_ID=<your-session-id>` in your shell."
        )
    })
}

async fn cli_get(path: &str) -> anyhow::Result<()> {
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!("http://localhost:{port}{path}");
    let resp = reqwest::get(&url).await?;
    let text = resp.text().await?;
    println!("{text}");
    Ok(())
}

async fn cli_post(path: &str, body: &serde_json::Value) -> anyhow::Result<()> {
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!("http://localhost:{port}{path}");
    let client = reqwest::Client::new();
    let resp = client.post(&url).json(body).send().await?;
    let text = resp.text().await?;
    println!("{text}");
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

    #[test]
    fn classify_http_response_404_surfaces_as_error() {
        use reqwest::StatusCode;
        let err = classify_http_response(StatusCode::NOT_FOUND, "{\"error\":\"pane 'x' is not registered\"}")
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
}
