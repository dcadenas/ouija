use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, GetPromptRequestParams, GetPromptResult, ListPromptsResult, Prompt,
    PromptArgument, PromptMessage, PromptMessageRole, ServerCapabilities, ServerInfo,
};
use rmcp::{RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::scheduler;
use crate::state::AppState;
use crate::tmux;

#[derive(Clone)]
pub struct OuijaMcp {
    state: Arc<AppState>,
    tool_router: ToolRouter<Self>,
}

impl OuijaMcp {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionRegisterParams {
    /// A short identifier for this session (e.g. "relay", "web", "api")
    pub id: String,
    /// tmux pane ID (e.g. "%42"). Auto-detected from unregistered Claude panes if omitted.
    pub pane: Option<String>,
    /// Whether this session has vim keybindings enabled. If true, text injection
    /// will enter INSERT mode first to avoid vim command interpretation.
    #[serde(default)]
    pub vim_mode: Option<bool>,
    /// The project directory this session is working in.
    pub project_dir: Option<String>,
    /// A short description of what this session is doing.
    pub role: Option<String>,
    /// Whether this session is visible to and reachable from remote nodes.
    /// Defaults to true if omitted.
    #[serde(default)]
    pub networked: Option<bool>,
    /// What this session needs, offers, or is working on.
    /// Used to discover collaboration opportunities with other sessions.
    pub bulletin: Option<String>,
    /// Claude Code conversation/session ID (UUID) for `--resume` on restart.
    /// If provided, restart will use `--resume <id>` instead of `--continue`.
    pub claude_session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionUnregisterParams {
    /// Session ID to unregister
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionSendParams {
    /// Your session ID (the sender)
    pub from: String,
    /// Target session ID
    pub to: String,
    /// Message to send
    pub message: String,
    /// Whether the sender expects a reply from the target.
    /// If true, the message prefix includes `?` and the daemon tracks the pending reply.
    pub expects_reply: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionUpdateParams {
    /// Session ID to update
    pub id: String,
    /// New role/focus description for this session
    pub role: Option<String>,
    /// Updated project directory
    pub project_dir: Option<String>,
    /// What this session needs, offers, or is working on.
    /// Used to discover collaboration opportunities with other sessions.
    pub bulletin: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClearPendingReplyParams {
    /// Your session ID
    pub session: String,
    /// The sender whose pending reply to clear
    pub from: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskCreateParams {
    /// Human-readable name for the task
    pub name: String,
    /// Cron expression (e.g. "*/5 * * * *"). Evaluated in UTC.
    pub cron: String,
    /// Optional: inject into this existing session (only for continue_session mode).
    /// When absent, the task name is used as the session name.
    pub target_session: Option<String>,
    /// Message to inject on each run
    pub message: String,
    /// Override project directory for session revival
    pub project_dir: Option<String>,
    /// If true, the task fires once then auto-deletes itself.
    #[serde(default)]
    pub once: Option<bool>,
    /// Claude session ID for --resume on revival (instead of --continue).
    pub claude_session_id: Option<String>,
    /// What happens each time the task fires.
    /// Variants: continue_session (default), new_session, persistent_worktree, disposable_worktree.
    /// For persistent_worktree, set clear_context to control conversation persistence.
    #[serde(default)]
    pub on_fire: Option<crate::scheduler::OnFire>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskDeleteParams {
    /// Task ID to delete (8-char hex)
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskIdParams {
    /// Task ID (8-char hex)
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionNameParams {
    /// Session name to operate on
    pub name: String,
    /// If true, start a fresh session (no --continue/--resume).
    #[serde(default)]
    pub fresh: Option<bool>,
    /// If true, run in an isolated git worktree (claude --worktree).
    #[serde(default)]
    pub worktree: Option<bool>,
    /// Project directory to open the session in.
    /// If omitted, derives from projects_dir + name.
    #[serde(default)]
    pub project_dir: Option<String>,
}

#[tool_router]
impl OuijaMcp {
    /// Register this Claude session with the ouija daemon.
    /// Also used to rename: if the pane is already registered under a different
    /// name, the old name is replaced and remote daemons are notified.
    #[tool(
        description = "Register this Claude session with the ouija daemon. You MUST provide the `pane` parameter. To get it, first run `echo $TMUX_PANE` in bash, then pass the result here."
    )]
    async fn session_register(
        &self,
        Parameters(params): Parameters<SessionRegisterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if params.id.contains('/') {
            return Ok(CallToolResult::error(vec![Content::text(
                "session ID must not contain '/'",
            )]));
        }

        let pane = match params.pane {
            Some(p) => Some(p),
            None => find_unregistered_pane(&self.state).await,
        };

        if pane.is_none() {
            return Ok(CallToolResult::error(vec![Content::text(
                "pane is required for message delivery. \
                 Run `echo $TMUX_PANE` in bash to get your pane ID, \
                 then call session_register again with the pane parameter.",
            )]));
        }

        if let Some(ref p) = pane {
            if !crate::tmux::pane_alive(p) {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "pane {p} does not exist — run `echo $TMUX_PANE` to get the correct pane ID"
                ))]));
            }
        }

        let project_description = params
            .project_dir
            .as_deref()
            .and_then(crate::api::extract_project_description);
        let metadata = crate::state::SessionMetadata {
            vim_mode: params.vim_mode.unwrap_or(false),
            project_dir: params.project_dir,
            role: params.role,
            bulletin: params.bulletin,
            networked: params.networked.unwrap_or(true),
            claude_session_id: params.claude_session_id,
            project_description,
            ..Default::default()
        };
        let result = self
            .state
            .register_session(params.id.clone(), pane, metadata)
            .await;

        let (session, replaced) = match result {
            crate::state::RegisterResult::Ok { session, replaced } => (session, replaced),
            crate::state::RegisterResult::Conflict { existing_pane } => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "session '{}' already registered on pane {existing_pane}",
                    params.id
                ))]));
            }
        };

        self.state
            .announce_and_activate(&session, replaced.as_deref())
            .await;

        tracing::info!(
            "registered session: {} (pane: {:?})",
            session.id,
            session.pane
        );

        let contents = vec![Content::text(format!("registered as {}", session.id))];

        Ok(CallToolResult::success(contents))
    }

    /// Unregister this session from the ouija daemon.
    #[tool(description = "Unregister a session from the ouija daemon")]
    async fn session_unregister(
        &self,
        Parameters(params): Parameters<SessionUnregisterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match self.state.remove_session(&params.id).await {
            Some(_) => {
                let msg = crate::protocol::WireMessage::SessionRemove {
                    id: params.id.clone(),
                    daemon_id: self.state.config.npub.clone(),
                    daemon_name: self.state.config.name.clone(),
                };
                crate::transport::broadcast(&self.state, &msg).await;
                tracing::info!("unregistered session: {}", params.id);
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "unregistered {}",
                    params.id
                ))]))
            }
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "session '{}' not found",
                params.id
            ))])),
        }
    }

    /// Update a session's role, project_dir, and/or bulletin without re-registering.
    #[tool(
        description = "Update a session's metadata (role, project_dir, bulletin) without re-registering. Use this to keep your session description fresh. Set `bulletin` to advertise what you need or can offer other sessions."
    )]
    async fn session_update(
        &self,
        Parameters(params): Parameters<SessionUpdateParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if params.role.is_none() && params.project_dir.is_none() && params.bulletin.is_none() {
            return Ok(CallToolResult::error(vec![Content::text(
                "at least one of role, project_dir, or bulletin must be provided",
            )]));
        }

        match self
            .state
            .update_session_metadata(&params.id, params.role, params.project_dir, params.bulletin)
            .await
        {
            Some(session) => {
                // Broadcast updated metadata to peers if networked
                if self.state.is_session_networked(&session) {
                    let msg = crate::protocol::WireMessage::SessionAnnounce {
                        id: session.id.clone(),
                        daemon_id: self.state.config.npub.clone(),
                        daemon_name: self.state.config.name.clone(),
                        metadata: Some(session.metadata.clone()),
                    };
                    crate::transport::broadcast(&self.state, &msg).await;
                }
                let contents = vec![Content::text(format!("updated session '{}'", session.id))];
                Ok(CallToolResult::success(contents))
            }
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "session '{}' not found or is remote",
                params.id
            ))])),
        }
    }

    /// Send a message to another Claude session. If the target is on this machine,
    /// it will be injected into their tmux pane. If remote, it goes over the network.
    #[tool(description = "Send a message to another Claude session")]
    async fn session_send(
        &self,
        Parameters(params): Parameters<SessionSendParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let sessions = self.state.sessions.read().await;
        let target = sessions.get(&params.to).cloned();
        drop(sessions);

        match target {
            Some(session) => {
                match &session.origin {
                    crate::state::SessionOrigin::Local => {
                        if let Some(pane) = &session.pane {
                            let formatted = tmux::format_session_message(
                                &params.from,
                                &params.message,
                                params.expects_reply,
                            );
                            let pane = pane.clone();
                            let vim_mode = session.metadata.vim_mode;
                            let lock = self.state.pane_lock(&pane);
                            let _guard = lock.lock().await;
                            match tokio::task::spawn_blocking(move || {
                                tmux::inject(&pane, &formatted, vim_mode)
                            })
                            .await
                            {
                                Ok(Ok(())) => {
                                    // Mark target as handling an injected message
                                    {
                                        let mut sessions = self.state.sessions.write().await;
                                        if let Some(s) = sessions.get_mut(&params.to) {
                                            s.block_interactive = true;
                                        }
                                    }
                                    if params.expects_reply {
                                        self.state.notify_agent(&params.to, crate::session_agent::SessionMsg::MessageDelivered {
                                        from: params.from.clone(),
                                        message: params.message.clone(),
                                        expects_reply: true,
                                    }).await;
                                    }
                                    // Clear any pending reply from sender to recipient (sender is replying)
                                    self.state
                                        .notify_agent(
                                            &params.from,
                                            crate::session_agent::SessionMsg::ReplySent {
                                                to: params.to.clone(),
                                            },
                                        )
                                        .await;
                                    self.state
                                        .log_message(
                                            params.from.clone(),
                                            params.to.clone(),
                                            params.message.clone(),
                                            true,
                                            "tmux",
                                        )
                                        .await;
                                    let mut contents = vec![Content::text("delivered")];
                                    append_staleness_hint(&self.state, &params.from, &mut contents)
                                        .await;
                                    Ok(CallToolResult::success(contents))
                                }
                                Ok(Err(e)) => Ok(CallToolResult::error(vec![Content::text(
                                    format!("tmux inject failed: {e}"),
                                )])),
                                Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                                    "task failed: {e}"
                                ))])),
                            }
                        } else {
                            Ok(CallToolResult::error(vec![Content::text(format!(
                                "session '{}' has no tmux pane",
                                params.to
                            ))]))
                        }
                    }
                    crate::state::SessionOrigin::Remote(_daemon_id) => {
                        let wire_to = crate::state::strip_remote_prefix(&params.to).to_string();
                        let wire_msg = crate::protocol::WireMessage::SessionSend {
                            from: params.from.clone(),
                            to: wire_to.clone(),
                            message: params.message.clone(),
                            expects_reply: params.expects_reply,
                        };
                        if crate::transport::broadcast(&self.state, &wire_msg).await {
                            // Clear pending reply using stripped name (inbound stores short name)
                            self.state
                                .notify_agent(
                                    &params.from,
                                    crate::session_agent::SessionMsg::ReplySent {
                                        to: wire_to.clone(),
                                    },
                                )
                                .await;
                            self.state
                                .log_message(
                                    params.from.clone(),
                                    params.to.clone(),
                                    params.message.clone(),
                                    true,
                                    "nostr",
                                )
                                .await;
                            let mut contents = vec![Content::text("sent via nostr")];
                            append_staleness_hint(&self.state, &params.from, &mut contents).await;

                            Ok(CallToolResult::success(contents))
                        } else {
                            Ok(CallToolResult::error(vec![Content::text(
                                "P2P not connected",
                            )]))
                        }
                    }
                    crate::state::SessionOrigin::Human(npub) => {
                        let formatted = crate::tmux::format_session_message(
                            &params.from,
                            &params.message,
                            params.expects_reply,
                        );
                        match crate::nostr_transport::send_plain_dm(&self.state, npub, &formatted)
                            .await
                        {
                            Ok(()) => {
                                self.state
                                    .log_message(
                                        params.from.clone(),
                                        params.to.clone(),
                                        params.message.clone(),
                                        true,
                                        "nostr-dm",
                                    )
                                    .await;
                                let mut contents = vec![Content::text("delivered via nostr DM")];
                                append_staleness_hint(&self.state, &params.from, &mut contents)
                                    .await;

                                Ok(CallToolResult::success(contents))
                            }
                            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                                "DM delivery failed: {e}"
                            ))])),
                        }
                    }
                }
            }
            None => {
                if let Some(new_id) = self.state.resolve_alias(&params.to).await {
                    Ok(CallToolResult::error(vec![Content::text(format!(
                        "session '{}' was renamed to '{}'. Retry with the new name.",
                        params.to, new_id
                    ))]))
                } else {
                    let suggestions =
                        crate::project_index::suggest_projects(&self.state, &params.to).await;
                    if suggestions.is_empty() {
                        Ok(CallToolResult::error(vec![Content::text(format!(
                            "session '{}' not found",
                            params.to
                        ))]))
                    } else {
                        let lines: Vec<String> = suggestions
                            .iter()
                            .map(|p| {
                                let desc = p
                                    .description
                                    .as_deref()
                                    .map(|d| format!(" — {d}"))
                                    .unwrap_or_default();
                                format!("  - {} ({}{})", p.name, p.dir.display(), desc)
                            })
                            .collect();
                        Ok(CallToolResult::error(vec![Content::text(format!(
                            "session '{}' not found. Matching projects:\n{}\n\
                             Use session_start to launch one.",
                            params.to,
                            lines.join("\n")
                        ))]))
                    }
                }
            }
        }
    }

    /// List all known sessions across all connected daemons.
    #[tool(description = "List all known Claude sessions across all connected daemons")]
    async fn session_list(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let sessions = self.state.sessions.read().await;
        let list: Vec<serde_json::Value> = sessions
            .values()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "pane": s.pane,
                    "origin": match &s.origin {
                        crate::state::SessionOrigin::Remote(d) => format!("remote({d})"),
                        other => other.label().to_string(),
                    },
                    "project_dir": s.metadata.project_dir,
                    "role": s.metadata.role,
                    "bulletin": s.metadata.bulletin,
                    "worktree": s.metadata.worktree,
                    "last_metadata_update": s.metadata.last_metadata_update,
                    "stale": s.metadata.is_stale(),
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string(&serde_json::json!({
                "daemon": self.state.config.name,
                "sessions": list,
            }))
            .unwrap(),
        )]))
    }

    /// Clear a pending reply when the sender's session is gone and you cannot reply normally.
    #[tool(
        description = "Clear a pending reply from an unreachable session. Use when session_send fails because the sender disconnected."
    )]
    async fn clear_pending_reply(
        &self,
        Parameters(params): Parameters<ClearPendingReplyParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.state
            .notify_agent(
                &params.session,
                crate::session_agent::SessionMsg::ClearPendingReply {
                    from: params.from.clone(),
                },
            )
            .await;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "cleared pending reply from '{}' on '{}'",
            params.from, params.session
        ))]))
    }

    /// List all scheduled tasks with their status, next/last run times, and run counts.
    #[tool(description = "List all scheduled tasks with status and run info")]
    async fn task_list(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let tasks = self.state.scheduled_tasks.read().await;
        let mut list: Vec<&scheduler::ScheduledTask> = tasks.values().collect();
        list.sort_by_key(|t| &t.created_at);
        let entries: Vec<serde_json::Value> = list
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "name": t.name,
                    "cron": t.cron,
                    "target_session": t.target_session,
                    "message": t.message,
                    "enabled": t.enabled,
                    "next_run": t.next_run,
                    "last_run": t.last_run,
                    "last_status": t.last_status,
                    "run_count": t.run_count,
                    "once": t.once,
                    "claude_session_id": t.claude_session_id,
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({ "tasks": entries })).unwrap(),
        )]))
    }

    /// Create a new scheduled task. The cron expression is evaluated in UTC.
    #[tool(description = "Create a new scheduled task. Cron expressions are evaluated in UTC.")]
    async fn task_create(
        &self,
        Parameters(params): Parameters<TaskCreateParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = scheduler::validate_cron(&params.cron) {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "invalid cron expression: {e}"
            ))]));
        }

        let task = scheduler::new_task(
            params.name,
            params.cron,
            params.target_session,
            params.message,
            params.project_dir,
            params.once.unwrap_or(false),
            params.claude_session_id,
            params.on_fire.unwrap_or_default(),
        );

        let id = task.id.clone();
        self.state.add_task(task).await;

        let contents = vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({ "created": id })).unwrap(),
        )];

        Ok(CallToolResult::success(contents))
    }

    /// Delete a scheduled task by its ID.
    #[tool(description = "Delete a scheduled task by ID")]
    async fn task_delete(
        &self,
        Parameters(params): Parameters<TaskDeleteParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match self.state.remove_task(&params.id).await {
            Some(_) => Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&serde_json::json!({ "deleted": params.id })).unwrap(),
            )])),
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "task '{}' not found",
                params.id
            ))])),
        }
    }

    /// Enable a previously disabled scheduled task so it resumes running on schedule.
    #[tool(description = "Enable a scheduled task so it runs on its cron schedule")]
    async fn task_enable(
        &self,
        Parameters(params): Parameters<TaskIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let exists = self
            .state
            .scheduled_tasks
            .read()
            .await
            .contains_key(&params.id);
        if !exists {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "task '{}' not found",
                params.id
            ))]));
        }
        self.state
            .update_task(&params.id, |t| {
                t.enabled = true;
                t.next_run = scheduler::compute_next_run(&t.cron);
            })
            .await;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "enabled": params.id
            }))
            .unwrap(),
        )]))
    }

    /// Disable a scheduled task so it stops running. The task is kept but won't fire until re-enabled.
    #[tool(description = "Disable a scheduled task so it stops running")]
    async fn task_disable(
        &self,
        Parameters(params): Parameters<TaskIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let exists = self
            .state
            .scheduled_tasks
            .read()
            .await
            .contains_key(&params.id);
        if !exists {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "task '{}' not found",
                params.id
            ))]));
        }
        self.state
            .update_task(&params.id, |t| {
                t.enabled = false;
                t.next_run = None;
            })
            .await;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "disabled": params.id
            }))
            .unwrap(),
        )]))
    }

    /// Trigger a scheduled task immediately, regardless of its cron schedule.
    /// Useful for testing or one-off execution.
    #[tool(description = "Trigger a scheduled task immediately, bypassing its cron schedule")]
    async fn task_trigger(
        &self,
        Parameters(params): Parameters<TaskIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let exists = self
            .state
            .scheduled_tasks
            .read()
            .await
            .contains_key(&params.id);
        if !exists {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "task '{}' not found",
                params.id
            ))]));
        }
        scheduler::execute_task(&self.state, &params.id).await;

        // Read back the updated task for status
        let tasks = self.state.scheduled_tasks.read().await;
        let status = tasks.get(&params.id).map(|t| {
            serde_json::json!({
                "triggered": params.id,
                "last_status": t.last_status,
            })
        });
        drop(tasks);

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&status.unwrap_or(serde_json::json!({
                "triggered": params.id
            })))
            .unwrap(),
        )]))
    }

    #[tool(
        description = "Gracefully stop a Claude session — sends /exit first, falls back to SIGTERM after 10s. Only use when the user explicitly asks to kill or stop a specific session. NEVER kill a session to work around a name conflict with session_start. Use node/name for remote sessions."
    )]
    async fn session_kill(
        &self,
        Parameters(params): Parameters<SessionNameParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = execute_command(&self.state, &params.name, "/kill").await;
        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Start a new Claude session in a tmux window. Directory is derived from projects_dir/<name> unless project_dir is specified. If a session with this name already exists, NEVER kill it — send it a message, or start a new session with a suffixed name (e.g. name-2) using project_dir pointing to the same repo and worktree=true. Use node/name to start on a remote machine."
    )]
    async fn session_start(
        &self,
        Parameters(params): Parameters<SessionNameParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = if params.name.contains('/') {
            execute_command(&self.state, &params.name, "/start").await
        } else {
            crate::nostr_transport::admin_start_session(
                &self.state,
                &params.name,
                params.worktree,
                params.project_dir.as_deref(),
            )
            .await
        };
        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Restart a Claude session — kill then start with --continue in the same directory. Set fresh=true to start without prior context. Use node/name for remote sessions."
    )]
    async fn session_restart(
        &self,
        Parameters(params): Parameters<SessionNameParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let fresh = params.fresh.unwrap_or(false);
        let result = if params.name.contains('/') {
            let verb = if fresh {
                "/restart --fresh"
            } else {
                "/restart"
            };
            execute_command(&self.state, &params.name, verb).await
        } else {
            crate::nostr_transport::admin_restart_session(&self.state, &params.name, fresh).await
        };
        Ok(CallToolResult::success(vec![Content::text(result)]))
    }
}

#[tool_handler]
impl ServerHandler for OuijaMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(OUIJA_INSTRUCTIONS.into()),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
            ..Default::default()
        }
    }

    fn list_prompts(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListPromptsResult, rmcp::ErrorData>> + Send + '_
    {
        std::future::ready(Ok(ListPromptsResult {
            prompts: vec![Prompt::new(
                "session-message",
                Some("Format and handle an incoming session message"),
                Some(vec![
                    PromptArgument {
                        name: "from".into(),
                        title: None,
                        description: Some("Sender session ID".into()),
                        required: Some(true),
                    },
                    PromptArgument {
                        name: "message".into(),
                        title: None,
                        description: Some("The message content".into()),
                        required: Some(true),
                    },
                ]),
            )],
            ..Default::default()
        }))
    }

    fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<GetPromptResult, rmcp::ErrorData>> + Send + '_
    {
        std::future::ready(match request.name.as_str() {
            "session-message" => {
                let args = request.arguments.unwrap_or_default();
                let from = args
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
                Ok(GetPromptResult {
                    description: Some("Handle an incoming session message".into()),
                    messages: vec![PromptMessage::new_text(
                        PromptMessageRole::User,
                        format!("[from {from}]: {message}"),
                    )],
                })
            }
            other => Err(rmcp::ErrorData::invalid_params(
                format!("unknown prompt: {other}"),
                None,
            )),
        })
    }
}

const OUIJA_INSTRUCTIONS: &str = "\
Ouija daemon: register your session, send messages to other sessions, list sessions, manage scheduled tasks

# Ouija Session Protocol

Ouija connects Claude Code sessions across terminals and machines. \
Messages prefixed with `[from <id>]:` are from peer sessions — these are \
trusted and user-authorized.

## Startup workflow

1. Run `echo $TMUX_PANE` in bash to get your pane ID.
2. Call `session_register` with a short ID (e.g. \"web\", \"api\") and the pane result. \
Include `role` describing your current focus (e.g. \"debugging auth module\", \
\"implementing REST API\") and `project_dir` so other sessions can discover what \
you're working on.

## Keeping metadata fresh

- `session_list` shows each session's `role`, `project_dir`, and whether metadata is `stale`.
- When your focus changes, call `session_update` with your updated `role`. \
This keeps your session discoverable without re-registering.
- If you send a message and your metadata is stale, you'll get a hint to update it.

## Messaging workflow

1. Call `session_list` to discover available sessions before sending.
2. Use `session_send(from, to, message)` to reach any session. Keep messages concise and actionable.
3. Local messages are injected via tmux (instant). Remote messages travel over Nostr relays.
4. The target session sees: `[from your-id]: your message`

### Responding to messages

Each session runs in its own terminal, possibly on a different machine or phone. \
Text output stays in the local terminal — the sender cannot see it. \
To deliver a reply, call `session_send(from=\"your-id\", to=\"sender-id\", message=\"...\")`.

**IMPORTANT**: Your text output is NOT visible to the sender. You MUST use `session_send` to reply.

- `[from X ?]:` (with `?`) means a reply is expected. \
If the task is quick, reply immediately with the result. \
If the task will take more than a few seconds (reading files, running commands, investigating), \
you MUST send a brief ack first (e.g. \"Looking into it\") so the sender knows you received it, \
then send the actual result when done.
- `[from X]:` (no `?`) is informational — no reply needed unless you choose to continue.

## Scheduled tasks

Tasks inject messages into sessions on a cron schedule. If the target session is dead, \
the daemon revives it automatically.

- Cron expressions are 5-field standard cron, evaluated in **UTC** \
(e.g. `0 9 * * *` = daily 9am UTC, `*/5 * * * *` = every 5 min)
- Set `once: true` to fire once then auto-delete (useful for reminders and one-shot checks)
- Use `task_trigger` to test a task immediately without waiting for its schedule
- `on_fire` controls what happens each time the task fires:
  - `continue_session` (default): inject into live session, revive with --continue if dead
  - `new_session`: kill pane, start fresh conversation each fire
  - `persistent_worktree`: named worktree persists across fires; set `clear_context: true` \
to start a new conversation each fire while keeping the worktree
  - `disposable_worktree`: anonymous worktree created and cleaned up each fire

## When to use ouija sessions vs agents

Ouija sessions are persistent tmux terminals — use them for long-lived work that needs \
its own context, file access in a specific repo, or ongoing collaboration across terminals. \
If the user just needs a quick answer or investigation, prefer the Agent tool (subagent) — \
it's lighter and doesn't consume a terminal.

When the user says \"create an agent\" or \"start an agent\" without mentioning \
\"session\" or \"ouija\", they likely mean a subagent (Agent tool), not an ouija session.

## Session lifecycle rules

- **NEVER kill an existing session to resolve a name conflict.** If `session_start` returns \
\"already exists\", send a message to the existing session instead, or start a new session \
with a suffixed name (e.g. `name-2`) using `project_dir` pointing to the same repo and \
`worktree=true`.
- **NEVER kill a session just to get a fresh one.** Use `session_restart` with `fresh=true` \
to restart cleanly, or start a separate worktree session alongside it.
- **Prefer messaging over spawning.** If a session already exists for a project, send it a \
message rather than starting a new one.
";

/// If the sender's metadata is stale, append a hint nudging them to update.
/// Execute a command locally or forward to a remote node.
///
/// If `name` contains a `/` (e.g. "macbook/crash-cache"), the command is
/// forwarded to the remote daemon. Otherwise it runs locally.
async fn execute_command(state: &Arc<AppState>, name: &str, verb: &str) -> String {
    if let Some((node_name, session_name)) = name.split_once('/') {
        // Find daemon_id for this node name
        let daemon_id = {
            let nodes = state.nodes.read().await;
            nodes
                .values()
                .find(|n| n.name == node_name)
                .map(|n| n.daemon_id.clone())
        };
        let Some(_daemon_id) = daemon_id else {
            return format!("node '{node_name}' not found");
        };

        let command = format!("{verb} {session_name}");
        let rx = state.register_pending_command(command.clone());
        let wire = crate::protocol::WireMessage::Command {
            command,
            daemon_id: state.config.npub.clone(),
        };
        if !crate::transport::broadcast(state, &wire).await {
            return "P2P not connected".to_string();
        }
        match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => "command channel closed".to_string(),
            Err(_) => "timeout waiting for remote response".to_string(),
        }
    } else {
        let state_arc = state.clone();
        crate::nostr_transport::handle_admin_command(&state_arc, &format!("{verb} {name}")).await
    }
}

async fn append_staleness_hint(state: &AppState, sender_id: &str, contents: &mut Vec<Content>) {
    let sessions = state.sessions.read().await;
    if let Some(session) = sessions.get(sender_id) {
        if session.metadata.is_stale() {
            contents.push(Content::text(
                "Hint: your session metadata is stale. \
                 Consider calling session_update with your current role \
                 so other sessions see what you're working on.",
            ));
        }
    }
}

/// Find an unregistered Claude pane to associate with a new session.
///
/// Scans all tmux panes running `claude` and returns one that isn't
/// already registered. Falls back to `None` if zero or multiple
/// candidates exist (ambiguous).
async fn find_unregistered_pane(state: &AppState) -> Option<String> {
    let claude_panes = tmux::find_claude_panes().ok()?;
    let sessions = state.sessions.read().await;
    let registered_panes: std::collections::HashSet<&str> = sessions
        .values()
        .filter_map(|s| s.pane.as_deref())
        .collect();

    let candidates: Vec<_> = claude_panes
        .iter()
        .filter(|p| !registered_panes.contains(p.pane_id.as_str()))
        .collect();

    if candidates.len() == 1 {
        Some(candidates[0].pane_id.clone())
    } else {
        None
    }
}
