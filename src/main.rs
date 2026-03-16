mod admin;
mod api;
mod config;
mod mcp;
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
    Start {
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
    /// Send a message through the daemon
    Send { to: String, message: String },
    /// Inject directly into a tmux pane
    Inject { pane: String, message: String },
    /// Rename a session
    Rename { old_id: String, new_id: String },
    /// Remove a session
    Remove { id: String },
    /// Stop the running daemon
    Stop,
    /// Print the message log file path
    LogPath {
        #[arg(long)]
        data: Option<String>,
    },
    /// Update ouija from crates.io and restart daemon
    Update,
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
    /// Add a human Nostr session
    AddHuman {
        /// The human's Nostr public key (npub1...)
        #[arg(long)]
        npub: String,
        /// Session name for this human
        #[arg(long)]
        name: String,
        /// Grant admin privileges
        #[arg(long)]
        admin: bool,
        /// Default session to route unprefixed messages to
        #[arg(long)]
        default_session: Option<String>,
    },
    /// Remove a human Nostr session
    RemoveHuman {
        /// Name of the human session to remove
        #[arg(long)]
        name: String,
    },
    /// List configured human sessions
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

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ouija=info".parse().expect("valid default filter")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Start {
            port,
            name,
            data,
            ticket,
            relays,
        } => {
            ensure_plugin_installed();

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

            let config = config::OuijaConfig::new(name, port, data, npub)?;
            let state = state::AppState::new(config);

            // Build project index in background
            let index_state = state.clone();
            tokio::spawn(async move {
                project_index::refresh_index(&index_state).await;
            });

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

                loop {
                    let interval = reaper_state.settings.read().await.reaper_interval_secs;
                    tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

                    // Reap dead local sessions and announce removals
                    let reaped = reaper_state.reap_dead_sessions().await;
                    if !reaped.is_empty() {
                        for id in &reaped {
                            let msg = crate::protocol::WireMessage::SessionRemove {
                                id: id.clone(),
                                daemon_id: reaper_state.config.npub.clone(),
                                daemon_name: reaper_state.config.name.clone(),
                            };
                            transport::broadcast(&reaper_state, &msg).await;
                        }
                    }

                    // If over the max session limit, close the most idle ones.
                    // Killing the pane lets the next reaper cycle clean up + broadcast.
                    for id in reaper_state.collect_excess_idle_sessions().await {
                        tracing::info!(
                            "auto-closing idle session '{id}' (over max_local_sessions)"
                        );
                        crate::nostr_transport::admin_kill_session(&reaper_state, &id).await;
                    }

                    // Scan tmux, update cache, auto-register unregistered panes
                    reaper_state.scan_and_autoregister_panes().await;

                    // Broadcast full session list only on startup or when it changes
                    let current_hash = reaper_state.local_session_hash().await;
                    if first_run || current_hash != last_session_hash {
                        transport::broadcast_local_sessions(&reaper_state).await;
                        last_session_hash = current_hash;
                        first_run = false;
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
        Command::Send { to, message } => {
            let from = resolve_my_session_id().await;
            match from {
                Some(id) => {
                    let body = serde_json::json!({ "to": to, "message": message, "from": id });
                    cli_post("/api/send", &body).await?;
                }
                None => {
                    let pane = std::env::var("TMUX_PANE").unwrap_or_default();
                    anyhow::bail!(
                        "no session registered for this pane ({pane}).\n\
                         Run `ouija register <name>` first."
                    );
                }
            }
        }
        Command::Inject { pane, message } => {
            let body = serde_json::json!({ "pane": pane, "message": message });
            cli_post("/api/inject", &body).await?;
        }
        Command::Rename { old_id, new_id } => {
            let body = serde_json::json!({ "old_id": old_id, "new_id": new_id });
            cli_post("/api/rename", &body).await?;
        }
        Command::Remove { id } => {
            let body = serde_json::json!({ "id": id });
            cli_post("/api/remove", &body).await?;
        }
        Command::Stop => {
            stop_daemon()?;
        }
        Command::LogPath { data } => {
            let config = config::OuijaConfig::new("_".into(), 0, data, String::new())?;
            println!("{}", config.data_dir.join("messages.jsonl").display());
        }
        Command::Update => {
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
                admin,
                default_session,
            }) => {
                if !npub.starts_with("npub1") {
                    anyhow::bail!("npub must start with 'npub1'");
                }
                let config_dir = config::OuijaConfig::default_config_dir();
                std::fs::create_dir_all(&config_dir)?;
                let mut settings = persistence::load_settings(&config_dir)?;
                if settings.human_sessions.iter().any(|h| h.name == name) {
                    anyhow::bail!("human session '{name}' already exists");
                }
                settings.human_sessions.push(persistence::HumanSession {
                    npub,
                    name: name.clone(),
                    admin,
                    default_session,
                    welcomed: false,
                });
                persistence::save_settings(&config_dir, &settings)?;
                println!("added human session '{name}'");
            }
            Some(ConfigAction::RemoveHuman { name }) => {
                let config_dir = config::OuijaConfig::default_config_dir();
                let mut settings = persistence::load_settings(&config_dir)?;
                let before = settings.human_sessions.len();
                settings.human_sessions.retain(|h| h.name != name);
                if settings.human_sessions.len() == before {
                    anyhow::bail!("human session '{name}' not found");
                }
                persistence::save_settings(&config_dir, &settings)?;
                println!("removed human session '{name}'");
            }
            Some(ConfigAction::ListHumans) => {
                let config_dir = config::OuijaConfig::default_config_dir();
                let settings = persistence::load_settings(&config_dir)?;
                if settings.human_sessions.is_empty() {
                    println!("no human sessions configured");
                } else {
                    println!("{:<12} {:<20} {:<8} DEFAULT", "NAME", "NPUB", "ADMIN");
                    for h in &settings.human_sessions {
                        let npub_short = if h.npub.len() > 16 {
                            format!("{}...", &h.npub[..16])
                        } else {
                            h.npub.clone()
                        };
                        let default = h.default_session.as_deref().unwrap_or("--");
                        println!(
                            "{:<12} {:<20} {:<8} {}",
                            h.name, npub_short, h.admin, default
                        );
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

    restore_persisted_sessions(state).await;
    register_human_sessions(state).await;

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
    let alive = tokio::task::spawn_blocking(move || {
        sessions
            .into_iter()
            .filter(|ps| ps.pane.as_ref().is_some_and(|p| crate::tmux::pane_alive(p)))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();

    if alive.is_empty() {
        return;
    }

    let mut state_sessions = state.sessions.write().await;
    for ps in &alive {
        let session = state::Session {
            id: ps.id.clone(),
            pane: ps.pane.clone(),
            origin: state::SessionOrigin::Local,
            registered_at: ps.registered_at,
            last_activity_at: ps.last_activity_at,
            metadata: ps.metadata.clone(),
            block_interactive: false,
        };
        state_sessions.insert(ps.id.clone(), session);
    }
    tracing::info!("restored {} persisted sessions", alive.len());
}

async fn register_human_sessions(state: &state::AppState) {
    let humans = state.settings.read().await.human_sessions.clone();
    if humans.is_empty() {
        return;
    }

    let mut sessions = state.sessions.write().await;
    for h in &humans {
        if sessions.contains_key(&h.name) {
            tracing::debug!("human session '{}' already registered", h.name);
            continue;
        }
        let session = state::Session {
            id: h.name.clone(),
            pane: None,
            origin: state::SessionOrigin::Human(h.npub.clone()),
            registered_at: chrono::Utc::now(),
            last_activity_at: chrono::Utc::now(),
            metadata: state::SessionMetadata {
                role: Some("human".to_string()),
                networked: false,
                ..Default::default()
            },
            block_interactive: false,
        };
        sessions.insert(h.name.clone(), session);
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

    if conns.is_empty() {
        return;
    }

    let Some(transport) = state.transport_by_name("nostr").await else {
        tracing::warn!("skipping node reconnection: nostr transport not active");
        return;
    };

    let mut reconnected = 0;
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

    if reconnected > 0 {
        tracing::info!("reconnected to {reconnected} persisted nodes");
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

    // Also kill any "ouija start" processes
    let pkill_killed = Cmd::new("pkill")
        .args(["-f", "ouija start"])
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
    if latest == current {
        println!("already on latest version ({current})");
        refresh_plugin_cache(&latest);
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

    refresh_plugin_cache(&latest);

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
        .arg("start")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn ouija start")?;

    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
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
            return Ok(());
        }
        if i == 19 {
            anyhow::bail!("daemon did not start within 10s");
        }
    }
    Ok(())
}

/// Refresh the Claude Code plugin cache from the source directory.
///
/// Refresh the plugin cache after an update. Tries the source directory first
/// (for local dev), falls back to embedded files (for production installs).
fn refresh_plugin_cache(version: &str) {
    let home = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h),
        Err(_) => return,
    };

    let cache_base = home.join(".claude/plugins/cache/ouija/ouija");
    let cache_dir = match std::fs::read_dir(&cache_base)
        .ok()
        .and_then(|mut entries| entries.next())
        .and_then(|e| e.ok())
    {
        Some(entry) => entry.path(),
        None => {
            // No cache dir yet — run full install with embedded files
            ensure_plugin_installed();
            return;
        }
    };

    // Try source directory first (local dev workflow)
    let source_synced = try_sync_from_source(&home, &cache_dir);

    if !source_synced {
        // Fall back to embedded files (production install via cargo)
        write_embedded_plugin_files(&cache_dir);
    }

    // Stamp version so hooks can detect plugin/daemon mismatch
    let _ = std::fs::write(cache_dir.join(".version"), version);

    println!("plugin cache refreshed");
}

/// Try to sync plugin files from the local source directory. Returns true if
/// a source dir was found and synced.
fn try_sync_from_source(home: &std::path::Path, cache_dir: &std::path::Path) -> bool {
    let settings_path = home.join(".claude/settings.json");
    let settings_str = match std::fs::read_to_string(&settings_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let settings: serde_json::Value = match serde_json::from_str(&settings_str) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let source_dir = match settings
        .pointer("/extraKnownMarketplaces/ouija/source/path")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
    {
        Some(d) if d.exists() => d,
        _ => return false,
    };

    for dir in &["scripts", "hooks", "skills"] {
        let src = source_dir.join(dir);
        let dst = cache_dir.join(dir);
        if src.is_dir() {
            if let Err(e) = sync_dir(&src, &dst) {
                eprintln!("warning: failed to sync plugin {dir}: {e}");
            }
        }
    }

    let src = source_dir.join(".mcp.json");
    let dst = cache_dir.join(".mcp.json");
    if src.is_file() {
        let _ = std::fs::copy(&src, &dst);
    }

    true
}

fn sync_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            sync_dir(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Write all embedded plugin files to the given cache directory.
fn write_embedded_plugin_files(cache_dir: &std::path::Path) {
    let files: &[(&str, &str)] = &[
        ("hooks/hooks.json", embedded::HOOKS_JSON),
        (".mcp.json", embedded::MCP_JSON),
        (
            "scripts/block-interactive-prompts.sh",
            embedded::SCRIPT_BLOCK_INTERACTIVE,
        ),
        (
            "scripts/check-pending-replies.sh",
            embedded::SCRIPT_CHECK_PENDING,
        ),
        (
            "scripts/clear-injection-marker.sh",
            embedded::SCRIPT_CLEAR_MARKER,
        ),
        ("scripts/ouija-register.sh", embedded::SCRIPT_REGISTER),
        ("scripts/ouija-statusline.sh", embedded::SCRIPT_STATUSLINE),
        ("scripts/ouija-unregister.sh", embedded::SCRIPT_UNREGISTER),
        ("scripts/session-diff.sh", embedded::SCRIPT_SESSION_DIFF),
        (
            "skills/ouija/SKILL.md",
            embedded::SKILLS_PEER_TRUST,
        ),
    ];

    for (path, content) in files {
        let dest = cache_dir.join(path);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&dest, content);
    }

    // Make scripts executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(entries) = std::fs::read_dir(cache_dir.join("scripts")) {
            for entry in entries.flatten() {
                let _ =
                    std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o755));
            }
        }
    }
}

// --- Embedded plugin files ---
// These are compiled into the binary so `ouija start` can bootstrap the Claude
// Code plugin without needing the source repo on disk.

mod embedded {
    pub const HOOKS_JSON: &str = include_str!("../hooks/hooks.json");
    pub const MCP_JSON: &str = include_str!("../.mcp.json");

    pub const SCRIPT_BLOCK_INTERACTIVE: &str =
        include_str!("../scripts/block-interactive-prompts.sh");
    pub const SCRIPT_CHECK_PENDING: &str = include_str!("../scripts/check-pending-replies.sh");
    pub const SCRIPT_CLEAR_MARKER: &str = include_str!("../scripts/clear-injection-marker.sh");
    pub const SCRIPT_REGISTER: &str = include_str!("../scripts/ouija-register.sh");
    pub const SCRIPT_STATUSLINE: &str = include_str!("../scripts/ouija-statusline.sh");
    pub const SCRIPT_UNREGISTER: &str = include_str!("../scripts/ouija-unregister.sh");
    pub const SCRIPT_SESSION_DIFF: &str = include_str!("../scripts/session-diff.sh");

    pub const SKILLS_PEER_TRUST: &str = include_str!("../skills/ouija/SKILL.md");
}

/// Ensure the Claude Code plugin is installed. Called on every `ouija start`.
/// If the plugin cache already exists, just stamps the version. If not, writes
/// all embedded files and registers in installed_plugins.json / settings.json.
fn ensure_plugin_installed() {
    let home = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h),
        Err(_) => return,
    };

    let claude_dir = home.join(".claude");
    if !claude_dir.exists() {
        // Claude Code not installed — skip silently
        return;
    }

    let version = env!("CARGO_PKG_VERSION");
    let cache_dir = claude_dir.join("plugins/cache/ouija/ouija/0.1.0");

    let needs_full_install = !cache_dir.exists();
    if needs_full_install {
        println!("installing Claude Code plugin...");
    }

    write_embedded_plugin_files(&cache_dir);

    // Stamp version
    let _ = std::fs::write(cache_dir.join(".version"), version);

    if !needs_full_install {
        return;
    }

    // --- First-time registration ---

    // Update installed_plugins.json
    let plugins_path = claude_dir.join("plugins/installed_plugins.json");
    let mut plugins: serde_json::Value = std::fs::read_to_string(&plugins_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| {
            serde_json::json!({
                "version": 2,
                "plugins": {}
            })
        });

    if !plugins["plugins"]
        .as_object()
        .is_some_and(|p| p.contains_key("ouija@ouija"))
    {
        let now = chrono::Utc::now().to_rfc3339();
        plugins["plugins"]["ouija@ouija"] = serde_json::json!([{
            "scope": "user",
            "installPath": cache_dir.to_string_lossy(),
            "version": "0.1.0",
            "installedAt": now,
            "lastUpdated": now,
            "isLocal": false
        }]);
        let _ = std::fs::write(
            &plugins_path,
            serde_json::to_string_pretty(&plugins).unwrap(),
        );
    }

    // Update settings.json — enable the plugin
    let settings_path = claude_dir.join("settings.json");
    let mut settings: serde_json::Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let mut changed = false;
    if let Some(obj) = settings.as_object_mut() {
        let enabled = obj
            .entry("enabledPlugins")
            .or_insert_with(|| serde_json::json!({}));
        if enabled.get("ouija@ouija").is_none() {
            enabled["ouija@ouija"] = serde_json::Value::Bool(true);
            changed = true;
        }

        // Set statusLine if not already configured
        if obj.get("statusLine").is_none() {
            let script = cache_dir.join("scripts/ouija-statusline.sh");
            obj.insert(
                "statusLine".to_string(),
                serde_json::json!({
                    "type": "command",
                    "command": script.to_string_lossy()
                }),
            );
            changed = true;
        }
    }

    if changed {
        let _ = std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        );
    }

    println!("Claude Code plugin installed. Restart Claude Code sessions to activate.");
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

/// Look up the registered session ID for the current tmux pane.
async fn resolve_my_session_id() -> Option<String> {
    let pane = std::env::var("TMUX_PANE").ok()?;
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
