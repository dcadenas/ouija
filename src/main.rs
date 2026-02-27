mod admin;
mod api;
mod config;
mod mcp;
mod nostr_transport;
mod persistence;
mod protocol;
mod scheduler;
mod server;
mod state;
mod tmux;
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
        /// Connect to a peer using an nprofile1 ticket
        #[arg(long)]
        ticket: Option<String>,
        /// Additional nostr relay URLs (repeatable)
        #[arg(long = "relay")]
        relays: Vec<String>,
    },
    /// Show daemon status
    Status,
    /// List connected and saved peers
    Peers,
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
    /// Connect to a peer daemon using a ticket
    Connect {
        ticket: String,
        /// Optional name to identify this peer
        #[arg(long)]
        name: Option<String>,
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
    /// Update ouija from GitHub Releases and restart daemon
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
        /// Target session ID
        target: String,
        /// Message to inject
        message: String,
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
            let name = name.unwrap_or_else(|| {
                hostname::get()
                    .ok()
                    .and_then(|h| h.into_string().ok())
                    .unwrap_or_else(|| "ouija".to_string())
            });

            // Load nostr keys early — the npub serves as the daemon's universal identity.
            // Data dir must be created before loading keys.
            let data_dir = data
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    let base = std::env::var("XDG_DATA_HOME")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|_| {
                            let home =
                                std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                            std::path::PathBuf::from(home).join(".local/share")
                        });
                    base.join("ouija")
                });
            std::fs::create_dir_all(&data_dir)?;
            let nostr_keys = nostr_transport::load_or_create_keys(&data_dir)?;
            let npub = nostr_keys
                .public_key()
                .to_bech32()
                .unwrap_or_else(|_| "unknown".into());
            tracing::info!("daemon identity: {npub}");

            let config = config::OuijaConfig::new(name, port, data, npub)?;
            let state = state::AppState::new(config);

            // Setup nostr transport in the background so HTTP starts immediately.
            let bg_state = state.clone();
            tokio::spawn(async move {
                setup_nostr_transport(&bg_state, ticket.as_deref(), relays).await;
            });

            // Reap dead sessions + sync session list every 30s
            let reaper_state = state.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;

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

                    // Periodic full session list broadcast for reconciliation
                    transport::broadcast_local_sessions(&reaper_state).await;
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
        Command::Peers => {
            let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
            let url = format!("http://localhost:{port}/api/peers");
            let resp: serde_json::Value = reqwest::get(&url).await?.json().await?;
            let peers = resp["peers"].as_array();
            match peers {
                Some(list) if !list.is_empty() => {
                    println!("{:<12} {:<12} {:<10} SINCE", "NAME", "STATUS", "TRANSPORT");
                    for p in list {
                        let name = p["name"].as_str().unwrap_or("-");
                        let status = p["status"].as_str().unwrap_or("unknown");
                        let transport = p["transport"].as_str().unwrap_or("-");
                        let since = p["since"].as_str().unwrap_or("-");
                        println!("{:<12} {:<12} {:<10} {}", name, status, transport, since);
                    }
                }
                _ => println!("no peers"),
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
                eprintln!("WARNING: This will destroy your nostr identity (nsec). All peers must re-connect.");
                eprintln!("Run with --yes to confirm.");
                std::process::exit(1);
            }
            let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
            let url =
                format!("http://localhost:{port}/api/regenerate-ticket?confirm=true");
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
                    let body =
                        serde_json::json!({ "to": to, "message": message, "from": id });
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
        },
        Command::Task { action } => match action {
            TaskAction::List => {
                let port =
                    std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
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
                            let target = t["target_session"].as_str().unwrap_or("-");
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
                let port =
                    std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
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
                let port =
                    std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
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
                            let target = r["target_session"].as_str().unwrap_or("-");
                            let status = r["status"].as_str().unwrap_or("-");
                            let err = r["error"].as_str().unwrap_or("");
                            println!("{:<22} {:<12} {:<10} {:<10} {}", ts, name, target, status, err);
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

    if let Some(ticket) = ticket
        && let Err(e) = transport.connect(ticket, state.clone(), true).await
    {
        tracing::warn!("failed to connect to ticket peer: {e}");
    }

    reconnect_persisted_peers(state.clone()).await;
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
            .filter(|ps| {
                ps.pane
                    .as_ref()
                    .is_some_and(|p| crate::tmux::pane_alive(p))
            })
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
            metadata: ps.metadata.clone(),
        };
        state_sessions.insert(ps.id.clone(), session);
    }
    tracing::info!("restored {} persisted sessions", alive.len());
}

async fn reconnect_persisted_peers(state: state::SharedState) {
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
        tracing::warn!("skipping peer reconnection: nostr transport not active");
        return;
    };

    let mut reconnected = 0;
    for conn in &conns {
        // Skip legacy (non-nostr) connections
        if !conn.ticket.starts_with("nprofile1") {
            tracing::info!("skipping legacy non-nostr connection");
            continue;
        }

        let label = match &conn.peer_name {
            Some(name) => name.clone(),
            None => "unnamed".to_string(),
        };

        // Skip duplicate connections to the same daemon
        let npub = conn
            .daemon_npub
            .clone()
            .or_else(|| crate::api::extract_npub(&conn.ticket));
        if let Some(ref npub) = npub {
            let peer_name = conn.peer_name.as_deref().unwrap_or(&npub[..16.min(npub.len())]);
            if let Err(existing) = state.try_add_peer(npub, peer_name) {
                tracing::info!("skipping duplicate connection to {label} (already connected as '{existing}')");
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
        tracing::info!("reconnected to {reconnected} persisted peers");
    }
}

fn stop_daemon() -> anyhow::Result<()> {
    use std::process::Command as Cmd;

    // Kill the ouija-daemon tmux session if it exists
    let tmux_killed = Cmd::new("tmux")
        .args(["kill-session", "-t", "ouija-daemon"])
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

const TARGET: &str = env!("TARGET");
const GITHUB_RELEASES_URL: &str =
    "https://api.github.com/repos/dcadenas/ouija/releases/latest";

fn update_and_restart() -> anyhow::Result<()> {
    use std::process::Command as Cmd;

    println!("fetching latest release from GitHub...");
    let output = Cmd::new("curl")
        .args(["-sf", GITHUB_RELEASES_URL])
        .output()
        .context("failed to query GitHub releases")?;

    let release: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("could not parse GitHub release JSON")?;

    let tag = release["tag_name"]
        .as_str()
        .context("no tag_name in release")?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    println!("latest release: {version} (target: {TARGET})");

    let tarball_name = format!("ouija-{version}-{TARGET}.tar.gz");
    let download_url = format!(
        "https://github.com/dcadenas/ouija/releases/download/{tag}/{tarball_name}"
    );

    let tmpdir = std::env::temp_dir().join(format!("ouija-update-{version}"));
    std::fs::create_dir_all(&tmpdir)?;

    println!("downloading {tarball_name}...");
    let download_status = Cmd::new("curl")
        .args(["-fL", &download_url, "-o"])
        .arg(tmpdir.join(&tarball_name))
        .status()
        .context("failed to download release")?;

    if !download_status.success() {
        let _ = std::fs::remove_dir_all(&tmpdir);
        anyhow::bail!(
            "no precompiled binary for {TARGET}.\n\
             Install manually with: cargo install ouija"
        );
    }

    println!("extracting...");
    let tar_status = Cmd::new("tar")
        .args(["xzf"])
        .arg(tmpdir.join(&tarball_name))
        .arg("-C")
        .arg(&tmpdir)
        .status()
        .context("failed to extract tarball")?;

    if !tar_status.success() {
        let _ = std::fs::remove_dir_all(&tmpdir);
        anyhow::bail!("failed to extract tarball");
    }

    let new_binary = tmpdir.join(format!("ouija-{version}-{TARGET}/ouija"));
    let current_exe = std::env::current_exe().context("can't determine current exe")?;

    println!("replacing {}", current_exe.display());
    std::fs::copy(&new_binary, &current_exe)
        .context("failed to replace binary")?;
    let _ = std::fs::remove_dir_all(&tmpdir);

    println!("restarting daemon...");
    stop_daemon()?;
    std::thread::sleep(std::time::Duration::from_secs(1));

    Cmd::new("tmux")
        .args(["new-session", "-d", "-s", "ouija-daemon", "ouija start"])
        .status()
        .context("failed to start ouija in tmux")?;

    for i in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if Cmd::new("curl")
            .args(["-sf", "http://localhost:7880/api/status"])
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            println!("ouija updated and running");
            return Ok(());
        }
        if i == 19 {
            anyhow::bail!("daemon did not start within 10s");
        }
    }
    Ok(())
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
