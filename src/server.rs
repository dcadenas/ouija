use axum::routing::{get, post};
use axum::Router;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::LocalSessionManager,
};
use tokio::net::TcpListener;

use crate::mcp::OuijaMcp;
use crate::state::SharedState;
use crate::{admin, api};

pub async fn run(state: SharedState) -> anyhow::Result<()> {
    let port = state.config.port;
    let name = state.config.name.clone();

    let mcp_state = state.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(OuijaMcp::new(mcp_state.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig {
            stateful_mode: false,
            ..Default::default()
        },
    );

    let app = Router::new()
        .nest_service("/mcp", mcp_service)
        .route("/admin", get(admin::dashboard))
        .route("/api/status", get(api::status))
        .route("/api/ticket", get(api::ticket))
        .route("/api/register", post(api::register))
        .route("/api/send", post(api::send_msg))
        .route("/api/inject", post(api::inject))
        .route("/api/rename", post(api::rename))
        .route("/api/remove", post(api::remove))
        .route("/api/connect", post(api::connect))
        .route("/api/peers", get(api::peers))
        .route("/api/regenerate-ticket", post(api::regenerate_ticket))
        .route("/api/settings", get(api::get_settings).post(api::update_settings))
        .route("/api/relays", get(api::get_relays).post(api::update_relays))
        .route("/api/tasks", get(api::list_tasks).post(api::create_task).delete(api::delete_task))
        .route("/api/tasks/enable", post(api::enable_task))
        .route("/api/tasks/disable", post(api::disable_task))
        .route("/api/tasks/trigger", post(api::trigger_task))
        .route("/api/task-runs", get(api::list_task_runs))
        .with_state(state);

    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("ouija daemon '{name}' listening on {addr}");
    tracing::info!("  MCP:   http://localhost:{port}/mcp");
    tracing::info!("  Admin: http://localhost:{port}/admin");
    axum::serve(listener, app).await?;

    Ok(())
}
