use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::SharedState;

/// Common request body for pane-identified hooks.
/// Accepts either `pane` (tmux pane ID like "%689") or `backend_session_id`
/// (opencode session UUID). At least one must be provided.
#[derive(Debug, Deserialize)]
pub struct PaneBody {
    #[serde(default)]
    pub pane: Option<String>,
    #[serde(default)]
    pub backend_session_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::daemon_protocol::deserialize_optional_incarnation"
    )]
    pub session_incarnation: Option<crate::daemon_protocol::SessionIncarnation>,
}

/// POST /api/hooks/session-end
pub async fn session_end(
    State(state): State<SharedState>,
    Json(body): Json<PaneBody>,
) -> (StatusCode, Json<Value>) {
    let result = session_end_inner(&state, body).await;
    let status = if result.get("error").is_some() {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };
    (status, Json(result))
}

async fn session_end_inner(
    state: &std::sync::Arc<crate::state::AppState>,
    body: PaneBody,
) -> Value {
    let Some(expected_incarnation) = body.session_incarnation else {
        return json!({ "skipped": "missing session incarnation" });
    };
    let session = {
        let proto = state.protocol.read().await;
        let found = proto
            .sessions
            .values()
            .find(|s| {
                s.metadata.session_incarnation == expected_incarnation
                    && (body
                        .pane
                        .as_deref()
                        .is_some_and(|p| s.pane.as_deref() == Some(p))
                        || body
                            .backend_session_id
                            .as_deref()
                            .is_some_and(|b| s.metadata.backend_session_id.as_deref() == Some(b)))
            })
            .cloned();
        match found {
            Some(s) => s,
            None => return json!({ "skipped": "no session" }),
        }
    };
    let id = session.id.clone();
    match state
        .dormant_owned(
            session.owner(),
            session.pane.clone(),
            chrono::Utc::now().timestamp(),
            crate::daemon_protocol::DormancySource::TrustedSessionEnd,
        )
        .await
    {
        crate::state::DormantOwnedOutcome::Dormant { .. } => json!({ "dormant": id }),
        crate::state::DormantOwnedOutcome::Removed { .. } => json!({ "removed": id }),
        crate::state::DormantOwnedOutcome::Superseded
        | crate::state::DormantOwnedOutcome::LifecycleInProgress => {
            json!({ "skipped": "session replaced" })
        }
        crate::state::DormantOwnedOutcome::PersistenceFailed => {
            json!({ "error": "failed to persist session dormancy" })
        }
    }
}

async fn exact_hook_session_owner(
    state: &std::sync::Arc<crate::state::AppState>,
    pane: Option<&str>,
    backend_session_id: Option<&str>,
    incarnation: Option<crate::daemon_protocol::SessionIncarnation>,
) -> Option<crate::daemon_protocol::ResourceOwner> {
    let incarnation = incarnation?;
    state
        .exact_hook_session_owner(pane, backend_session_id, incarnation)
        .await
}

/// POST /api/hooks/stop
pub async fn hook_stop(
    State(state): State<SharedState>,
    Json(body): Json<PaneBody>,
) -> (StatusCode, Json<Value>) {
    let result = hook_stop_inner(&state, body).await;
    (StatusCode::OK, Json(result))
}

async fn hook_stop_inner(state: &std::sync::Arc<crate::state::AppState>, body: PaneBody) -> Value {
    if let Some(owner) = exact_hook_session_owner(
        state,
        body.pane.as_deref(),
        body.backend_session_id.as_deref(),
        body.session_incarnation,
    )
    .await
    {
        state
            .notify_agent_owned(&owner, crate::session_agent::SessionMsg::Stopped)
            .await;
    }
    json!({ "ok": true })
}

/// POST /api/hooks/prompt-submit
pub async fn prompt_submit(
    State(state): State<SharedState>,
    Json(body): Json<PaneBody>,
) -> (StatusCode, Json<Value>) {
    let result = prompt_submit_inner(&state, body).await;
    (StatusCode::OK, Json(result))
}

async fn prompt_submit_inner(
    state: &std::sync::Arc<crate::state::AppState>,
    body: PaneBody,
) -> Value {
    // The prompt-submit hook no longer injects mesh state into the LLM
    // context window. We still notify the session agent that the session
    // is active (to reset idle / watchdog timers).
    if let Some(owner) = exact_hook_session_owner(
        state,
        body.pane.as_deref(),
        body.backend_session_id.as_deref(),
        body.session_incarnation,
    )
    .await
    {
        state
            .notify_agent_owned(&owner, crate::session_agent::SessionMsg::Active)
            .await;
    }
    json!({ "output": "" })
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // tool_name used by Deserialize; will be read when blocking logic is implemented
pub struct PreToolUseBody {
    #[serde(default)]
    pub pane: Option<String>,
    #[serde(default)]
    pub backend_session_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::daemon_protocol::deserialize_optional_incarnation"
    )]
    pub session_incarnation: Option<crate::daemon_protocol::SessionIncarnation>,
    #[serde(default)]
    pub tool_name: Option<String>,
}

/// POST /api/hooks/pre-tool-use
pub async fn pre_tool_use(
    State(state): State<SharedState>,
    Json(body): Json<PreToolUseBody>,
) -> (StatusCode, Json<Value>) {
    let result = pre_tool_use_inner(&state, body).await;
    (StatusCode::OK, Json(result))
}

async fn pre_tool_use_inner(
    state: &std::sync::Arc<crate::state::AppState>,
    body: PreToolUseBody,
) -> Value {
    // Treat any tool invocation as session activity: cancel the idle timer
    // so long sequences of tool calls within a single turn don't trigger
    // spurious idle-check nudges.
    if let Some(owner) = exact_hook_session_owner(
        state,
        body.pane.as_deref(),
        body.backend_session_id.as_deref(),
        body.session_incarnation,
    )
    .await
    {
        state
            .notify_agent_owned(&owner, crate::session_agent::SessionMsg::Active)
            .await;
    }
    // TODO: check injection marker state on the session to decide blocking.
    // Currently always allows interactive tools.
    json!({ "block": false })
}

/// POST /api/hooks/post-compact
pub async fn post_compact(
    State(state): State<SharedState>,
    Json(body): Json<PaneBody>,
) -> (StatusCode, Json<Value>) {
    let result = post_compact_inner(&state, body).await;
    (StatusCode::OK, Json(result))
}

async fn post_compact_inner(
    state: &std::sync::Arc<crate::state::AppState>,
    body: PaneBody,
) -> Value {
    let owner = match exact_hook_session_owner(
        state,
        body.pane.as_deref(),
        body.backend_session_id.as_deref(),
        body.session_incarnation,
    )
    .await
    {
        Some(id) => id,
        None => return json!({ "ok": true, "continuation_injected": false }),
    };

    // Drain the pending continuation from the agent (RPC — atomically take + clear)
    let continuation = state.drain_agent_compact_continuation_owned(&owner).await;

    let Some(continuation) = continuation else {
        return json!({ "ok": true, "continuation_injected": false });
    };

    // Look up pane for injection
    let pane = {
        let proto = state.protocol.read().await;
        proto
            .sessions
            .get(&owner.session_id)
            .filter(|session| session.owner() == owner)
            .and_then(|session| session.pane.clone())
    };
    let Some(pane) = pane else {
        return json!({ "ok": true, "continuation_injected": false, "error": "no pane" });
    };

    if let Err(e) =
        crate::tmux::locked_inject_owned(state, &owner, &pane, &continuation, false).await
    {
        tracing::warn!(
            session = %owner.session_id,
            incarnation = %owner.incarnation,
            "post-compact continuation injection failed: {e}"
        );
        return json!({ "ok": false, "error": e.to_string() });
    }

    json!({ "ok": true, "continuation_injected": true })
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionStartBody {
    /// Empty for a paneless hook running under a shared app-server.
    #[serde(default)]
    pub pane: String,
    pub cwd: String,
    #[serde(default)]
    pub backend_session_id: Option<String>,
    /// Generic backend identity supplied by adapters that cannot rely on a
    /// tmux pane. Kept distinct from the legacy adapter/session fields so a
    /// paneless claimant must present the same typed contract used by CLI/API.
    #[serde(default)]
    pub backend_identity: Option<crate::backend::BackendSessionIdentity>,
    /// Backend adapter reported by the hook. This is descriptive only; it is
    /// not generation-bound authorization for an existing pane mutation.
    #[serde(default)]
    pub adapter: Option<String>,
    /// The Ouija session id injected into a pane at managed launch time. It is
    /// accepted as proof only as part of the complete identity + credential +
    /// incarnation tuple handled above the uncredentialed discovery path.
    #[serde(default)]
    pub launch_session_id: Option<String>,
    #[serde(default)]
    pub launch_credential: Option<String>,
    /// Exact daemon-issued incarnation exported into this backend process.
    #[serde(
        default,
        deserialize_with = "crate::daemon_protocol::deserialize_optional_incarnation"
    )]
    pub session_incarnation: Option<crate::daemon_protocol::SessionIncarnation>,
}

/// POST /api/hooks/session-start
pub async fn session_start(
    State(state): State<SharedState>,
    Json(body): Json<SessionStartBody>,
) -> (StatusCode, Json<Value>) {
    let result = session_start_inner(&state, body).await;
    (StatusCode::OK, Json(result))
}

/// Mesh onboarding text surfaced to a freshly-registered session, or empty.
///
/// Claude Code and OpenCode auto-load the `ouija` skill, so they need nothing
/// here (returning empty keeps their SessionStart output unchanged). Codex also
/// gets the skill installed under `$CODEX_HOME/skills/ouija` (#1445), but the
/// static skill cannot know the session's live public id, so its register hook
/// still wraps this text into SessionStart `additionalContext`. `public_id` is
/// the session's resolved public Ouija id, taught as `--from` because Codex's
/// bash tool cannot be relied on to carry `TMUX_PANE` for sender resolution.
fn mesh_instructions_for_backend(backend: Option<&str>, public_id: &str) -> String {
    if backend != Some("codex-cli") {
        return String::new();
    }
    format!(
        "You are on the Ouija mesh. Message other sessions with the `ouija` CLI \
         (NOT your own messaging tools — they cannot reach the mesh).\n\
         Your public Ouija id is `{public_id}`. Pass it as `--from {public_id}` on \
         every command so the mesh knows who is sending.\n\n\
         - `ouija ls` — list reachable sessions (targets for messages).\n\
         - `ouija ask <target> \"question\" --from {public_id}` — send a question that \
         expects a reply; the command returns after delivery.\n\
         - `ouija tell <target> \"note\" --from {public_id}` — fire-and-forget message.\n\
         - `ouija reply <target> <msg-id> \"answer\" --from {public_id}` — answer a \
         `<msg ... reply=\"true\">` you received (the sender is blocked until you reply).\n\n\
         For generated or multi-line message text, use `--stdin` instead of putting the \
         message in shell quotes.\n\n\
         Incoming messages arrive as `<msg from=\"...\" id=\"N\" reply=\"true\">text</msg>`; \
         reply to those with `reply=\"true\"` using their `id`. Replies to your asks are pushed \
         into this session later as `<msg ... re=\"N\">...</msg>`. If that reply is your only \
         remaining blocker, end your turn and wait for the pushed message; do not poll the \
         message log, status, or pane output unless you are debugging suspected delivery failure."
    )
}

/// Confirm that an existing pane's hook claim still belongs to its registered
/// project. A SessionStart payload can inherit `TMUX_PANE` from another
/// assistant process, so pane identity alone is not sufficient to authorize a
/// backend-thread update.
async fn existing_pane_identity_matches(
    state: &std::sync::Arc<crate::state::AppState>,
    pane: &str,
    hook_cwd: &str,
    registered_project_dir: Option<&str>,
    registered_canonical_project: Option<&str>,
) -> bool {
    let Ok(hook_project) = crate::project_identity::resolve_project_identity_async(hook_cwd).await
    else {
        return false;
    };
    let Some(registered_project_dir) = registered_project_dir else {
        tracing::warn!(
            pane,
            hook_cwd,
            "session-start rejected: existing pane has no project directory"
        );
        return false;
    };
    let registered_canonical_project = if let Some(registered_canonical_project) =
        registered_canonical_project
    {
        registered_canonical_project.to_string()
    } else {
        let Ok(registered_project) =
            crate::project_identity::resolve_project_identity_async(registered_project_dir).await
        else {
            return false;
        };
        registered_project.canonical_repository
    };
    if registered_canonical_project != hook_project.canonical_repository {
        tracing::warn!(
            pane,
            hook_cwd,
            registered_project_dir,
            "session-start rejected: hook cwd does not match existing pane project"
        );
        return false;
    }

    live_pane_identity_matches(state, pane, hook_cwd).await
}

async fn live_pane_identity_matches(
    state: &std::sync::Arc<crate::state::AppState>,
    pane: &str,
    hook_cwd: &str,
) -> bool {
    let Ok(hook_project) = crate::project_identity::resolve_project_identity_async(hook_cwd).await
    else {
        return false;
    };
    let panes = state.list_assistant_panes().await;
    let Some(live_pane_path) = panes
        .iter()
        .find(|candidate| candidate.pane_id == pane)
        .and_then(|candidate| candidate.pane_current_path.as_deref())
    else {
        tracing::warn!(
            pane,
            "session-start rejected: existing pane is not a live assistant pane"
        );
        return false;
    };
    let Ok(live_project) =
        crate::project_identity::resolve_project_identity_async(live_pane_path).await
    else {
        return false;
    };
    if live_project.canonical_repository != hook_project.canonical_repository {
        tracing::warn!(
            pane,
            hook_cwd,
            live_pane_path,
            "session-start rejected: hook cwd does not match live pane cwd"
        );
        return false;
    }

    true
}

async fn session_start_inner(
    state: &std::sync::Arc<crate::state::AppState>,
    body: SessionStartBody,
) -> Value {
    // A complete managed proof is sufficient to select the atomic bind path.
    // An app-server can inherit an unrelated pane, so pane/cwd may corroborate
    // ordinary registration but must never decide ownership of this launch.
    if let (Some(identity), Some(launch_id), Some(credential), Some(incarnation)) = (
        body.backend_identity.as_ref(),
        body.launch_session_id.as_deref(),
        body.launch_credential.as_deref(),
        body.session_incarnation,
    ) {
        let identity = crate::backend::BackendSessionIdentity {
            backend: identity.backend.trim().to_string(),
            session_id: identity.session_id.trim().to_string(),
        };
        if identity.backend.is_empty() || identity.session_id.is_empty() {
            return json!({
                "skipped": "paneless SessionStart requires a complete backend identity",
                "output": "",
            });
        }
        let result = state
            .bind_backend_identity(launch_id, &identity, Some(credential), Some(incarnation))
            .await;
        return match result.outcome {
            crate::daemon_protocol::BackendIdentityBindOutcome::Bound { session_id }
            | crate::daemon_protocol::BackendIdentityBindOutcome::AlreadyBound { session_id } => {
                let backend = state
                    .protocol
                    .read()
                    .await
                    .sessions
                    .get(&session_id)
                    .and_then(|session| session.metadata.backend.as_deref())
                    .map(String::from);
                json!({
                    "registered": session_id,
                    "session_incarnation": incarnation.to_string(),
                    "output": mesh_instructions_for_backend(backend.as_deref(), launch_id),
                })
            }
            outcome => json!({
                "skipped": format!("paneless SessionStart backend identity rejected: {outcome:?}"),
                "output": "",
            }),
        };
    }

    if body.pane.trim().is_empty() {
        return json!({
            "skipped": "paneless SessionStart requires backend identity, launch session id, and launch credential",
            "output": "",
        });
    }

    let backend_identity = match body.backend_identity.as_ref() {
        Some(identity) => {
            let identity = crate::backend::BackendSessionIdentity {
                backend: identity.backend.trim().to_string(),
                session_id: identity.session_id.trim().to_string(),
            };
            if identity.backend.is_empty() || identity.session_id.is_empty() {
                return json!({
                    "skipped": "session-start requires a complete backend identity",
                    "output": "",
                });
            }
            if body
                .adapter
                .as_deref()
                .is_some_and(|adapter| adapter != identity.backend)
                || normalize_backend_session_id(body.backend_session_id.as_deref())
                    .is_some_and(|session_id| session_id != identity.session_id)
            {
                return json!({
                    "skipped": "session-start backend identity mismatch",
                    "output": "",
                });
            }
            Some(identity)
        }
        None => None,
    };
    let detected_backend = state.detect_backend_in_pane(&body.pane).await;
    if let (Some(identity), Some(detected_backend)) =
        (backend_identity.as_ref(), detected_backend.as_deref())
        && identity.backend != detected_backend
    {
        return json!({
            "skipped": "session-start backend identity does not match live pane",
            "output": "",
        });
    }

    let mut attestation_generation = None;
    if let Some(identity) = backend_identity.as_ref() {
        let Ok(project) = crate::project_identity::resolve_project_identity_async(&body.cwd).await
        else {
            return json!({
                "skipped": "invalid session-start project identity",
                "output": "",
            });
        };
        if !live_pane_identity_matches(state, &body.pane, &project.project_dir).await {
            return json!({
                "skipped": "session-start pane identity mismatch",
                "output": "",
            });
        }
        match state
            .reclaim_missing_backend_pane(identity, &body.pane, &project, body.session_incarnation)
            .await
        {
            crate::state::MissingBackendPaneReclaimOutcome::Reclaimed(owner)
            | crate::state::MissingBackendPaneReclaimOutcome::Current(owner) => {
                let output =
                    mesh_instructions_for_backend(Some(&identity.backend), &owner.session_id);
                return json!({
                    "registered": owner.session_id,
                    "session_incarnation": owner.incarnation.to_string(),
                    "output": output,
                });
            }
            crate::state::MissingBackendPaneReclaimOutcome::IncarnationMismatch => {
                return json!({
                    "skipped": "existing pane incarnation mismatch",
                    "output": "",
                });
            }
            crate::state::MissingBackendPaneReclaimOutcome::NotFound => {
                match state
                    .recover_dormant_session(identity, &body.pane, &project)
                    .await
                {
                    crate::state::DormantRecoveryOutcome::Recovered(owner)
                    | crate::state::DormantRecoveryOutcome::Current(owner) => {
                        let output = mesh_instructions_for_backend(
                            Some(&identity.backend),
                            &owner.session_id,
                        );
                        return json!({
                            "registered": owner.session_id,
                            "session_incarnation": owner.incarnation.to_string(),
                            "output": output,
                        });
                    }
                    crate::state::DormantRecoveryOutcome::NotFound => {}
                    crate::state::DormantRecoveryOutcome::Refused => {
                        return json!({
                            "skipped": "dormant identity recovery rejected",
                            "output": "",
                        });
                    }
                    crate::state::DormantRecoveryOutcome::PersistenceFailed => {
                        return json!({
                            "error": "dormant identity recovery persistence failed",
                            "output": "",
                        });
                    }
                }
            }
            crate::state::MissingBackendPaneReclaimOutcome::Refused => {
                return json!({
                    "skipped": "stale canonical identity reclaim rejected",
                    "output": "",
                });
            }
        }
        if let Some(existing_id) = state.find_session_by_pane(&body.pane).await {
            let incumbent_owner = {
                let protocol = state.protocol.read().await;
                protocol
                    .sessions
                    .get(&existing_id)
                    .map(|session| session.owner())
            };
            if let Some(incumbent_owner) = incumbent_owner {
                let basename = std::path::Path::new(&project.project_dir)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unnamed");
                let base_id = crate::state::sanitize_session_id(basename);
                if !base_id.is_empty() {
                    let replacement_id = {
                        let protocol = state.protocol.read().await;
                        crate::state::resolve_unique_session_id(
                            &protocol.sessions,
                            &protocol.dormant_sessions,
                            &base_id,
                            None,
                        )
                    };
                    match state
                        .replace_reused_cross_backend_pane(
                            incumbent_owner,
                            body.pane.clone(),
                            project.clone(),
                            identity.clone(),
                            replacement_id,
                        )
                        .await
                    {
                        crate::state::ReusedPaneReplacementOutcome::Replaced(owner) => {
                            let output = mesh_instructions_for_backend(
                                Some(&identity.backend),
                                &owner.session_id,
                            );
                            return json!({
                                "registered": owner.session_id,
                                "session_incarnation": owner.incarnation.to_string(),
                                "output": output,
                            });
                        }
                        crate::state::ReusedPaneReplacementOutcome::PersistenceFailed => {
                            return json!({
                                "error": "cross-backend pane replacement persistence failed",
                                "output": "",
                            });
                        }
                        crate::state::ReusedPaneReplacementOutcome::NotApplicable
                        | crate::state::ReusedPaneReplacementOutcome::Refused => {}
                    }
                }
            }
        }
        if matches!(identity.backend.as_str(), "codex-cli" | "claude-code")
            && state.find_session_by_pane(&body.pane).await.is_none()
        {
            match state
                .record_local_backend_pane_attestation(identity, &body.pane, &project)
                .await
            {
                crate::state::LocalBackendPaneAttestationRecordOutcome::Recorded(attestation) => {
                    attestation_generation = Some(attestation.generation);
                }
                crate::state::LocalBackendPaneAttestationRecordOutcome::Ambiguous { .. } => {
                    return json!({
                        "skipped": "session-start backend identity is ambiguous across live panes",
                        "output": "",
                    });
                }
                crate::state::LocalBackendPaneAttestationRecordOutcome::Rejected => {
                    return json!({
                        "skipped": "session-start backend pane attestation rejected",
                        "output": "",
                    });
                }
            }
        }
    }

    // Uncredentialed discovery remains subject to the operator's legacy
    // auto-registration policy. Trusted explicit-pane callbacks above retain
    // their transient attestation even when discovery is disabled.
    if !state.settings.read().await.auto_register {
        return json!({ "skipped": "auto_register disabled", "output": "" });
    }

    // Skip if pane already registered (Ouija-launched / API-started sessions hit
    // this path — they are pane-registered with their backend before the hook
    // fires). Still surface mesh onboarding for codex-cli here, keyed off the
    // session's authoritative stored backend + id, so the primary launch path
    // gets it (claude-code/opencode carry the skill and stay empty).
    if let Some(existing_id) = state.find_session_by_pane(&body.pane).await {
        let (
            existing_owner,
            existing_backend,
            registered_project_dir,
            registered_canonical_project,
            existing_backend_session_id,
        ) = {
            let proto = state.protocol.read().await;
            proto
                .sessions
                .get(&existing_id)
                .map(|session| {
                    (
                        session.owner(),
                        session.metadata.backend.clone(),
                        session.metadata.project_dir.clone(),
                        session.metadata.canonical_project_identity.clone(),
                        session.metadata.backend_session_id.clone(),
                    )
                })
                .expect("pane lookup returned an existing session")
        };
        if body
            .session_incarnation
            .is_some_and(|incarnation| incarnation != existing_owner.incarnation)
        {
            return json!({
                "skipped": "existing pane incarnation mismatch",
                "output": "",
            });
        }
        if !existing_pane_identity_matches(
            state,
            &body.pane,
            &body.cwd,
            registered_project_dir.as_deref(),
            registered_canonical_project.as_deref(),
        )
        .await
        {
            return json!({
                "skipped": "existing pane identity mismatch",
                "output": "",
            });
        }
        if let Some(backend_session_id) =
            normalize_backend_session_id(body.backend_session_id.as_deref())
        {
            let existing_backend_session_id =
                normalize_backend_session_id(existing_backend_session_id.as_deref());
            if existing_backend_session_id.as_deref() != Some(backend_session_id.as_str()) {
                tracing::warn!(
                    pane = body.pane,
                    session = existing_id,
                    stored_backend = ?existing_backend,
                    reported_adapter = ?body.adapter,
                    stored_backend_session_id = ?existing_backend_session_id,
                    reported_backend_session_id = backend_session_id,
                    "session-start rejected: existing pane backend mutation lacks generation-bound managed proof"
                );
                return json!({
                    "skipped": "existing pane backend generation proof required",
                    "output": "",
                });
            }
        }
        let bound_backend = {
            let proto = state.protocol.read().await;
            proto
                .sessions
                .get(&existing_id)
                .and_then(|session| session.metadata.backend.clone())
        };
        let output = mesh_instructions_for_backend(bound_backend.as_deref(), &existing_id);
        return json!({
            "registered": existing_id,
            "session_incarnation": existing_owner.incarnation.to_string(),
            "output": output,
        });
    }

    // Derive the live worktree and canonical repository from cwd.
    let Ok(project) = crate::project_identity::resolve_project_identity_async(&body.cwd).await
    else {
        return json!({ "error": "could not resolve project identity", "output": "" });
    };

    // Refuse a home-cwd registration: an agent whose cwd is still $HOME is a
    // premature hook mis-fire (e.g. opencode's SessionStart firing before it
    // cd's into its worktree). Registering it leaks a generic basename($HOME)-N
    // session that owns the live pane forever (#1483). The authoritative name
    // arrives via the API session_start path once the pane is bound.
    if crate::state::is_home_project_root(&project.project_dir) {
        return json!({ "skipped": "home cwd (premature session-start)", "output": "" });
    }

    let basename = std::path::Path::new(&project.project_dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unnamed");
    let base_id = crate::state::sanitize_session_id(basename);
    if base_id.is_empty() {
        return json!({ "error": "could not derive session name", "output": "" });
    }

    // Resolve name conflicts via the shared helper (same suffix-bumping and
    // same-pane-idempotency rules as scan_and_autoregister_panes).
    let id = {
        let proto = state.protocol.read().await;
        crate::state::resolve_unique_session_id(
            &proto.sessions,
            &proto.dormant_sessions,
            &base_id,
            Some(&body.pane),
        )
    };

    // Detect backend from the process running in the pane
    let backend = backend_identity
        .as_ref()
        .map(|identity| identity.backend.clone())
        .or(detected_backend);

    // Prefer the identity supplied by the backend's SessionStart adapter.
    // OpenCode has no such hook, so retain its shared-serve lookup fallback.
    let backend_session_id = match backend_identity.as_ref() {
        Some(identity) => Some(identity.session_id.clone()),
        None => match normalize_backend_session_id(body.backend_session_id.as_deref()) {
            Some(session_id) => Some(session_id),
            None if backend.as_deref() == Some("opencode") => {
                resolve_opencode_session_id(state, &project.project_dir).await
            }
            None => None,
        },
    };

    // Compute mesh onboarding text before `detected_backend` is moved into the
    // metadata. Non-empty only for codex-cli (Claude/opencode carry the skill).
    let output = mesh_instructions_for_backend(backend.as_deref(), &id);

    // Register
    let role = format!("working on {basename}");
    let proto_meta = crate::daemon_protocol::SessionMeta {
        project_dir: Some(project.project_dir),
        canonical_project_identity: Some(project.canonical_repository),
        role: Some(role),
        backend,
        backend_session_id,
        ..Default::default()
    };
    let effects = state
        .apply_and_execute(crate::daemon_protocol::Event::RegisterIfPaneUnbound {
            id: id.clone(),
            pane: body.pane.clone(),
            expected_backend_session_id: proto_meta.backend_session_id.clone(),
            expected_orphaned_marker_owner: None,
            metadata: proto_meta,
        })
        .await;
    let Some(owner) = effects.iter().find_map(|effect| match effect {
        crate::daemon_protocol::Effect::RegisterOk { owner, .. } if owner.session_id == id => {
            Some(owner)
        }
        _ => None,
    }) else {
        return json!({
            "skipped": "registration rejected",
            "output": "",
        });
    };
    if let (Some(identity), Some(generation)) = (backend_identity.as_ref(), attestation_generation)
    {
        state
            .consume_local_backend_pane_attestation(identity, generation)
            .await;
    }

    json!({
        "registered": id,
        "session_incarnation": owner.incarnation.to_string(),
        "output": output,
    })
}

fn normalize_backend_session_id(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
}

/// Query the opencode serve to find the most recently updated session for a
/// given project directory.  Returns the session ID if found.
async fn resolve_opencode_session_id(
    state: &std::sync::Arc<crate::state::AppState>,
    project_dir: &str,
) -> Option<String> {
    let port = state.opencode_serve_port();
    let url = format!("http://127.0.0.1:{port}/session");
    let resp = state
        .http_client
        .get(&url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let sessions: Vec<serde_json::Value> = resp.json().await.ok()?;
    // Find the most recently updated session matching this directory.
    sessions
        .iter()
        .filter(|s| s["directory"].as_str() == Some(project_dir))
        .max_by_key(|s| {
            s["time"]["updated"]
                .as_i64()
                .or_else(|| s["time"]["created"].as_i64())
                .unwrap_or(0)
        })
        .and_then(|s| s["id"].as_str().map(String::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn replace_session_on_same_pane(
        state: &std::sync::Arc<crate::state::AppState>,
        session_id: &str,
        pane: &str,
    ) {
        state
            .apply_and_execute(crate::daemon_protocol::Event::Remove {
                id: session_id.into(),
                keep_worktree: true,
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: session_id.into(),
                pane: Some(pane.into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
    }

    #[tokio::test]
    async fn dormant_conflict_session_start_suffixes_a_reserved_automatic_name() {
        let state = crate::state::AppState::new_for_test();
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("worker");
        std::fs::create_dir(&project).unwrap();
        let project = project.to_string_lossy().into_owned();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%old".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some(project.clone()),
                    canonical_project_identity: Some(project.clone()),
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("parked-thread".into()),
                    ..Default::default()
                },
            })
            .await;
        let owner = state.protocol.read().await.sessions["worker"].owner();
        state
            .apply_and_execute(crate::daemon_protocol::Event::DormantOwned {
                owner,
                expected_pane: Some("%old".into()),
                observed_at: 30,
                source: crate::daemon_protocol::DormancySource::Reaped,
            })
            .await;
        *state.cached_assistant_panes.write().await = vec![crate::tmux::TmuxPane {
            pane_id: "%new".into(),
            session_name: "ouija".into(),
            pane_current_path: Some(project.clone()),
            process_name: Some("claude".into()),
        }];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%new".into(),
                cwd: project,
                backend_session_id: None,
                backend_identity: None,
                adapter: None,
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(result["registered"], "worker-2");
        let protocol = state.protocol.read().await;
        assert!(protocol.dormant_sessions.contains_key("worker"));
        assert!(protocol.sessions.contains_key("worker-2"));
    }

    async fn session_incarnation(
        state: &std::sync::Arc<crate::state::AppState>,
        session_id: &str,
    ) -> crate::daemon_protocol::SessionIncarnation {
        state.protocol.read().await.sessions[session_id]
            .metadata
            .session_incarnation
    }

    #[tokio::test]
    async fn paneless_session_start_binds_only_credentialed_named_launch() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "managed".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    session_start_credential: Some("proof".into()),
                    ..Default::default()
                },
            })
            .await;

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: String::new(),
                cwd: "/same-checkout".into(),
                backend_session_id: Some("thread-1".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "codex-cli".into(),
                    session_id: "thread-1".into(),
                }),
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("managed".into()),
                launch_credential: Some("proof".into()),
                session_incarnation: Some(session_incarnation(&state, "managed").await),
            },
        )
        .await;

        assert_eq!(result["registered"], "managed");
        {
            let protocol = state.protocol.read().await;
            let metadata = &protocol.sessions["managed"].metadata;
            assert_eq!(metadata.backend_session_id.as_deref(), Some("thread-1"));
            assert!(
                metadata.session_start_credential.is_none(),
                "a successful paneless claim consumes its proof"
            );
        }

        let replay = session_start_inner(
            &state,
            SessionStartBody {
                pane: String::new(),
                cwd: "/same-checkout".into(),
                backend_session_id: Some("thread-1".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "codex-cli".into(),
                    session_id: "thread-1".into(),
                }),
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("managed".into()),
                launch_credential: Some("proof".into()),
                session_incarnation: Some(session_incarnation(&state, "managed").await),
            },
        )
        .await;
        assert_eq!(
            replay["registered"], "managed",
            "duplicate delivery is idempotent"
        );
    }

    #[tokio::test]
    async fn proven_launch_ignores_an_inherited_sibling_pane_and_replay_is_idempotent() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sibling".into(),
                pane: Some("%sibling".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/same-checkout".into()),
                    backend: Some("codex-cli".into()),
                    ..Default::default()
                },
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "intended".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/same-checkout".into()),
                    backend: Some("codex-cli".into()),
                    session_start_credential: Some("proof".into()),
                    ..Default::default()
                },
            })
            .await;
        let body = SessionStartBody {
            pane: "%sibling".into(),
            cwd: "/same-checkout".into(),
            backend_session_id: Some("thread-intended".into()),
            backend_identity: Some(crate::backend::BackendSessionIdentity {
                backend: "codex-cli".into(),
                session_id: "thread-intended".into(),
            }),
            adapter: Some("codex-cli".into()),
            launch_session_id: Some("intended".into()),
            launch_credential: Some("proof".into()),
            session_incarnation: Some(session_incarnation(&state, "intended").await),
        };
        assert_eq!(
            session_start_inner(&state, body.clone()).await["registered"],
            "intended"
        );
        assert_eq!(
            session_start_inner(&state, body).await["registered"],
            "intended"
        );
        let protocol = state.protocol.read().await;
        assert_eq!(
            protocol.sessions["intended"]
                .metadata
                .backend_session_id
                .as_deref(),
            Some("thread-intended")
        );
        assert!(
            protocol.sessions["sibling"]
                .metadata
                .backend_session_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn paneless_session_start_rejects_missing_launch_proof() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "managed".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    session_start_credential: Some("proof".into()),
                    ..Default::default()
                },
            })
            .await;

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: String::new(),
                cwd: "/same-checkout".into(),
                backend_session_id: Some("thread-1".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "codex-cli".into(),
                    session_id: "thread-1".into(),
                }),
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("managed".into()),
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "paneless SessionStart requires backend identity, launch session id, and launch credential"
        );
        assert!(
            state.protocol.read().await.sessions["managed"]
                .metadata
                .backend_session_id
                .is_none(),
            "unproven paneless starts must fail closed"
        );
    }

    #[tokio::test]
    async fn paneless_session_start_rejects_payload_b_with_incomplete_ambient_a_fields() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "launch-a".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    session_start_credential: Some("proof-a".into()),
                    ..Default::default()
                },
            })
            .await;

        // This represents thread B's payload after the static hook has
        // discarded inherited launch-A credential material. A launch id alone
        // cannot select the paneless managed binding path.
        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: String::new(),
                cwd: "/same-checkout".into(),
                backend_session_id: Some("thread-b".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "codex-cli".into(),
                    session_id: "thread-b".into(),
                }),
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("launch-a".into()),
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "paneless SessionStart requires backend identity, launch session id, and launch credential"
        );
        assert!(
            state.protocol.read().await.sessions["launch-a"]
                .metadata
                .backend_session_id
                .is_none(),
            "thread B must not bind launch A without an explicit credential"
        );
    }

    #[tokio::test]
    async fn paneless_session_start_cannot_claim_a_same_checkout_sibling_launch() {
        let state = crate::state::AppState::new_for_test();
        for (id, credential) in [("worker-a", "proof-a"), ("worker-b", "proof-b")] {
            state
                .apply_and_execute(crate::daemon_protocol::Event::Register {
                    id: id.into(),
                    pane: None,
                    metadata: crate::daemon_protocol::SessionMeta {
                        project_dir: Some("/same-checkout".into()),
                        backend: Some("codex-cli".into()),
                        session_start_credential: Some(credential.into()),
                        ..Default::default()
                    },
                })
                .await;
        }

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: String::new(),
                cwd: "/same-checkout".into(),
                backend_session_id: Some("thread-a".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "codex-cli".into(),
                    session_id: "thread-a".into(),
                }),
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("worker-a".into()),
                launch_credential: Some("proof-b".into()),
                session_incarnation: Some(session_incarnation(&state, "worker-a").await),
            },
        )
        .await;

        assert!(result.get("registered").is_none());
        let protocol = state.protocol.read().await;
        for id in ["worker-a", "worker-b"] {
            assert!(
                protocol.sessions[id].metadata.backend_session_id.is_none(),
                "same checkout must not substitute the sibling's proof"
            );
        }
    }

    fn assistant_pane(pane_id: &str, cwd: &str) -> crate::tmux::TmuxPane {
        assistant_pane_with_process(pane_id, cwd, "codex")
    }

    fn assistant_pane_with_process(
        pane_id: &str,
        cwd: &str,
        process_name: &str,
    ) -> crate::tmux::TmuxPane {
        crate::tmux::TmuxPane {
            pane_id: pane_id.into(),
            session_name: "test".into(),
            pane_current_path: Some(cwd.into()),
            process_name: Some(process_name.into()),
        }
    }

    #[tokio::test]
    async fn session_end_incomplete_row_is_removed() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "test-session".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        assert!(state.find_session_by_pane("%99").await.is_some());

        let body = PaneBody {
            pane: Some("%99".into()),
            backend_session_id: None,
            session_incarnation: Some(session_incarnation(&state, "test-session").await),
        };
        let result = session_end_inner(&state, body).await;
        assert!(result.get("removed").is_some());
        assert!(state.find_session_by_pane("%99").await.is_none());
    }

    #[tokio::test]
    async fn session_end_parks_eligible_clean_exit() {
        let config = crate::state::tests::test_config();
        let state = crate::state::AppState::new(config.clone());
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "claude-worker".into(),
                pane: Some("%90".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/claude-worker".into()),
                    canonical_project_identity: Some("/tmp/repository".into()),
                    backend: Some("claude-code".into()),
                    backend_session_id: Some("claude-session-1".into()),
                    role: Some("continuity work".into()),
                    ..Default::default()
                },
            })
            .await;
        let owner = state.protocol.read().await.sessions["claude-worker"].owner();

        let result = session_end_inner(
            &state,
            PaneBody {
                pane: Some("%90".into()),
                backend_session_id: Some("claude-session-1".into()),
                session_incarnation: Some(owner.incarnation),
            },
        )
        .await;

        assert_eq!(result["dormant"], "claude-worker");
        let protocol = state.protocol.read().await;
        assert!(!protocol.sessions.contains_key("claude-worker"));
        assert_eq!(
            protocol.dormant_sessions["claude-worker"].prior_owner,
            owner
        );
        assert_eq!(
            protocol.dormant_sessions["claude-worker"].source,
            crate::daemon_protocol::DormancySource::TrustedSessionEnd
        );
        drop(protocol);
        assert!(
            crate::persistence::load_sessions(&config.data_dir)
                .unwrap()
                .dormant_sessions
                .contains_key("claude-worker")
        );
    }

    #[tokio::test]
    async fn session_end_clean_exit_recovers_on_replacement_pane() {
        let project = tempfile::tempdir().unwrap();
        let project_dir = project
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "arbitrary-claude-id".into(),
                pane: Some("%90".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some(project_dir.clone()),
                    canonical_project_identity: Some(project_dir.clone()),
                    backend: Some("claude-code".into()),
                    backend_session_id: Some("claude-session-1".into()),
                    role: Some("continuity work".into()),
                    ..Default::default()
                },
            })
            .await;
        let prior_owner = state.protocol.read().await.sessions["arbitrary-claude-id"].owner();
        assert_eq!(
            session_end_inner(
                &state,
                PaneBody {
                    pane: Some("%90".into()),
                    backend_session_id: Some("claude-session-1".into()),
                    session_incarnation: Some(prior_owner.incarnation),
                },
            )
            .await["dormant"],
            "arbitrary-claude-id"
        );
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane_with_process("%91", &project_dir, "claude")];
        state.set_dormant_recovery_test_inspection(crate::tmux::ManagedPaneInspection::Unmanaged);

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%91".into(),
                cwd: project_dir,
                backend_session_id: Some("claude-session-1".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "claude-code".into(),
                    session_id: "claude-session-1".into(),
                }),
                adapter: Some("claude-code".into()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(result["registered"], "arbitrary-claude-id");
        let recovered = state.protocol.read().await.sessions["arbitrary-claude-id"].clone();
        assert_eq!(recovered.pane.as_deref(), Some("%91"));
        assert_eq!(recovered.metadata.role.as_deref(), Some("continuity work"));
        assert!(recovered.owner().incarnation > prior_owner.incarnation);
    }

    #[tokio::test]
    async fn session_end_lifecycle_lease_skips_without_mutation() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "leased".into(),
                pane: Some("%90".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/leased".into()),
                    canonical_project_identity: Some("/tmp/repository".into()),
                    backend: Some("claude-code".into()),
                    backend_session_id: Some("claude-session-lease".into()),
                    ..Default::default()
                },
            })
            .await;
        let owner = state.protocol.read().await.sessions["leased"].owner();
        assert_eq!(
            state.claim_existing_start(&owner).await.unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        let before = state.protocol.read().await.clone();

        let result = session_end_inner(
            &state,
            PaneBody {
                pane: Some("%90".into()),
                backend_session_id: Some("claude-session-lease".into()),
                session_incarnation: Some(owner.incarnation),
            },
        )
        .await;

        assert_eq!(result["skipped"], "session replaced");
        assert_eq!(*state.protocol.read().await, before);
    }

    #[tokio::test]
    async fn session_end_active_accounting_matches_reaping() {
        let observed_at = chrono::Utc::now().timestamp();
        let metadata = crate::daemon_protocol::SessionMeta {
            project_dir: Some("/tmp/accounting".into()),
            canonical_project_identity: Some("/tmp/repository".into()),
            backend: Some("claude-code".into()),
            backend_session_id: Some("claude-session-accounting".into()),
            fresh_context_after_active_secs: Some(60),
            active_context_accumulated_secs: 10,
            active_context_segment_started_at: Some(observed_at.saturating_sub(5)),
            ..Default::default()
        };
        let clean_exit = crate::state::AppState::new_for_test();
        clean_exit
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%90".into()),
                metadata: metadata.clone(),
            })
            .await;
        let clean_owner = clean_exit.protocol.read().await.sessions["worker"].owner();
        assert_eq!(
            session_end_inner(
                &clean_exit,
                PaneBody {
                    pane: Some("%90".into()),
                    backend_session_id: Some("claude-session-accounting".into()),
                    session_incarnation: Some(clean_owner.incarnation),
                },
            )
            .await["dormant"],
            "worker"
        );
        let clean_dormant = clean_exit.protocol.read().await.dormant_sessions["worker"].clone();

        let reaped = crate::state::AppState::new_for_test();
        reaped
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%90".into()),
                metadata,
            })
            .await;
        let reaped_owner = reaped.protocol.read().await.sessions["worker"].owner();
        assert_eq!(
            reaped
                .dormant_owned(
                    reaped_owner,
                    Some("%90".into()),
                    clean_dormant.dormant_at,
                    crate::daemon_protocol::DormancySource::Reaped,
                )
                .await,
            crate::state::DormantOwnedOutcome::Dormant {
                id: "worker".into()
            }
        );
        let reaped_metadata = reaped.protocol.read().await.dormant_sessions["worker"]
            .metadata
            .clone();

        assert_eq!(clean_dormant.metadata, reaped_metadata);
    }

    #[tokio::test]
    async fn session_end_persistence_failure_keeps_live_owner_and_reports_error() {
        let config = crate::state::tests::test_config();
        let state = crate::state::AppState::new(config.clone());
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%90".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/worker".into()),
                    canonical_project_identity: Some("/tmp/repository".into()),
                    backend: Some("claude-code".into()),
                    backend_session_id: Some("claude-session-failure".into()),
                    ..Default::default()
                },
            })
            .await;
        let owner = state.protocol.read().await.sessions["worker"].owner();
        assert!(
            state
                .notify_agent_owned(&owner, crate::session_agent::SessionMsg::Active)
                .await
        );
        let before = state.protocol.read().await.clone();
        std::fs::create_dir(config.data_dir.join("sessions.tmp")).unwrap();

        let (status, Json(result)) = session_end(
            State(state.clone()),
            Json(PaneBody {
                pane: Some("%90".into()),
                backend_session_id: Some("claude-session-failure".into()),
                session_incarnation: Some(owner.incarnation),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(result["error"], "failed to persist session dormancy");
        assert!(result.get("dormant").is_none());
        assert!(result.get("removed").is_none());
        assert_eq!(*state.protocol.read().await, before);
        assert!(
            state
                .notify_agent_owned(&owner, crate::session_agent::SessionMsg::Active)
                .await
        );
    }

    #[tokio::test]
    async fn legacy_tokenless_session_end_fails_closed() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "fresh".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let body = PaneBody {
            pane: Some("%99".into()),
            backend_session_id: None,
            session_incarnation: None,
        };
        let result = session_end_inner(&state, body).await;
        assert_eq!(result["skipped"], "missing session incarnation");
        assert!(state.find_session_by_pane("%99").await.is_some());
    }

    #[tokio::test]
    async fn stale_session_end_does_not_remove_same_pane_replacement() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let stale_incarnation = state.protocol.read().await.sessions["worker"]
            .metadata
            .session_incarnation;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Remove {
                id: "worker".into(),
                keep_worktree: true,
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;

        let result = session_end_inner(
            &state,
            PaneBody {
                pane: Some("%99".into()),
                backend_session_id: None,
                session_incarnation: Some(stale_incarnation),
            },
        )
        .await;

        assert_eq!(result["skipped"], "no session");
        assert!(state.find_session_by_pane("%99").await.is_some());
    }

    #[tokio::test]
    async fn session_end_no_session() {
        let state = crate::state::AppState::new_for_test();
        let body = PaneBody {
            pane: Some("%999".into()),
            backend_session_id: None,
            session_incarnation: None,
        };
        let result = session_end_inner(&state, body).await;
        assert!(result.get("skipped").is_some());
    }

    #[tokio::test]
    async fn hook_stop_no_session_returns_ok() {
        let state = crate::state::AppState::new_for_test();
        let body = PaneBody {
            pane: Some("%999".into()),
            backend_session_id: None,
            session_incarnation: None,
        };
        let result = hook_stop_inner(&state, body).await;
        assert_eq!(result, json!({ "ok": true }));
    }

    #[tokio::test]
    async fn hook_stop_does_not_notify_replacement_after_authorization() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%42".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let incarnation = session_incarnation(&state, "worker").await;
        let owner = exact_hook_session_owner(&state, Some("%42"), None, Some(incarnation))
            .await
            .expect("the original hook must authorize");

        replace_session_on_same_pane(&state, "worker", "%42").await;

        assert!(
            !state
                .notify_agent_owned(&owner, crate::session_agent::SessionMsg::Stopped)
                .await,
            "a stop hook authorized for the old incarnation must not reach the replacement agent"
        );
    }

    #[tokio::test]
    async fn activity_hook_does_not_notify_replacement_after_authorization() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%42".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let incarnation = session_incarnation(&state, "worker").await;
        let owner = exact_hook_session_owner(&state, Some("%42"), None, Some(incarnation))
            .await
            .expect("the original hook must authorize");

        replace_session_on_same_pane(&state, "worker", "%42").await;

        assert!(
            !state
                .notify_agent_owned(&owner, crate::session_agent::SessionMsg::Active)
                .await,
            "activity authorized for the old incarnation must not reach the replacement agent"
        );
    }

    #[tokio::test]
    async fn staged_hook_requires_exact_fallback_pane_and_receiver() {
        // Break caught: a staged incarnation must not authorize the stale
        // incumbent pane once the lease publishes a different inert pane.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%old".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let incumbent = state.protocol.read().await.sessions["worker"].owner();
        assert_eq!(
            state.claim_existing_start(&incumbent).await.unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        let target = match state
            .stage_restart_launch(
                &incumbent,
                "claude-code".into(),
                true,
                true,
                Some(60),
                None,
                None,
            )
            .await
        {
            crate::daemon_protocol::StageFreshLaunchOutcome::Staged { incarnation } => {
                crate::daemon_protocol::ResourceOwner {
                    session_id: "worker".into(),
                    incarnation,
                }
            }
            other => panic!("expected staged restart, got {other:?}"),
        };
        assert_eq!(
            state
                .record_inert_start_pane(&incumbent, target.clone(), "%fallback".into())
                .await
                .unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );

        assert_eq!(
            exact_hook_session_owner(&state, Some("%fallback"), None, Some(target.incarnation))
                .await,
            Some(target.clone())
        );
        assert_eq!(
            exact_hook_session_owner(&state, Some("%old"), None, Some(target.incarnation)).await,
            None
        );
        assert!(
            state
                .notify_agent_owned(&target, crate::session_agent::SessionMsg::Active)
                .await,
            "the exact fallback-pane hook must have a matching receiver"
        );
    }

    #[tokio::test]
    async fn staged_paneless_hook_requires_exact_backend_lease_claim_and_receiver() {
        // Break caught: a soft OpenCode target cannot authorize hooks from its
        // incarnation or backend ID independently of the exact lease claim.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_old".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        let incumbent = state.protocol.read().await.sessions["worker"].owner();
        assert_eq!(
            state.claim_existing_start(&incumbent).await.unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        let target = match state
            .stage_restart_launch(
                &incumbent,
                "opencode".into(),
                true,
                true,
                Some(60),
                None,
                None,
            )
            .await
        {
            crate::daemon_protocol::StageFreshLaunchOutcome::Staged { incarnation } => {
                crate::daemon_protocol::ResourceOwner {
                    session_id: "worker".into(),
                    incarnation,
                }
            }
            other => panic!("expected staged restart, got {other:?}"),
        };

        assert_eq!(
            exact_hook_session_owner(&state, None, Some("ses_new"), Some(target.incarnation)).await,
            None,
            "the target incarnation alone must not authorize an unclaimed backend"
        );
        assert_eq!(
            state
                .record_restart_backend_claim(
                    &incumbent,
                    &target,
                    "opencode".into(),
                    "ses_new".into(),
                )
                .await
                .unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        assert_eq!(
            exact_hook_session_owner(&state, None, Some("ses_new"), Some(target.incarnation)).await,
            Some(target.clone())
        );
        assert_eq!(
            exact_hook_session_owner(&state, None, Some("ses_old"), Some(target.incarnation)).await,
            None
        );
        assert_eq!(
            exact_hook_session_owner(&state, None, Some("ses_new"), Some(incumbent.incarnation))
                .await,
            None
        );
        assert!(
            state
                .notify_agent_owned(&target, crate::session_agent::SessionMsg::Active)
                .await,
            "the exact backend-claim hook must have a paneless target receiver"
        );
    }

    #[tokio::test]
    async fn prompt_submit_returns_empty_for_unknown_pane() {
        let state = crate::state::AppState::new_for_test();
        let body = PaneBody {
            pane: Some("%999".into()),
            backend_session_id: None,
            session_incarnation: None,
        };
        let result = prompt_submit_inner(&state, body).await;
        assert_eq!(result["output"], "");
    }

    #[tokio::test]
    async fn pre_tool_use_no_session_allows() {
        let state = crate::state::AppState::new_for_test();
        let body = PreToolUseBody {
            pane: Some("%999".into()),
            backend_session_id: None,
            session_incarnation: None,
            tool_name: Some("AskUserQuestion".into()),
        };
        let result = pre_tool_use_inner(&state, body).await;
        assert_eq!(result["block"], false);
    }

    #[tokio::test]
    async fn pre_tool_use_signals_activity_for_registered_session() {
        // Regression test for ouija#10: PreToolUse must reset the idle timer
        // by sending SessionMsg::Active to the session agent. We verify by
        // registering a session, arming its idle timer via Stopped (with a
        // configured reminder so the arm actually happens), then calling
        // pre_tool_use_inner and asserting the reminder never fires within
        // the timeout window.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "tool-activity".into(),
                pane: Some("%42".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    reminder: Some("keep working".into()),
                    ..Default::default()
                },
            })
            .await;
        state.settings.write().await.idle_timeout_secs = 1;

        // Arm the idle timer.
        state
            .notify_agent("tool-activity", crate::session_agent::SessionMsg::Stopped)
            .await;

        // Halfway through the idle window, a tool fires — should reset timer.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let body = PreToolUseBody {
            pane: Some("%42".into()),
            backend_session_id: None,
            session_incarnation: Some(session_incarnation(&state, "tool-activity").await),
            tool_name: Some("Bash".into()),
        };
        let result = pre_tool_use_inner(&state, body).await;
        assert_eq!(result["block"], false);

        // Give the agent time to process Active.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Session should no longer be marked idle. We can't easily observe
        // the timer directly, but we can check that notify_agent resolved
        // the session (i.e. find_session_by_pane still works).
        assert!(state.find_session_by_pane("%42").await.is_some());
    }

    #[tokio::test]
    async fn pre_tool_use_accepts_backend_session_id() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-session".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend_session_id: Some("oc-uuid-123".into()),
                    ..Default::default()
                },
            })
            .await;
        let body = PreToolUseBody {
            pane: None,
            backend_session_id: Some("oc-uuid-123".into()),
            session_incarnation: Some(session_incarnation(&state, "oc-session").await),
            tool_name: Some("bash".into()),
        };
        let result = pre_tool_use_inner(&state, body).await;
        assert_eq!(result["block"], false);
    }

    #[test]
    fn mesh_instructions_only_for_codex() {
        // The static skill can't know Codex's live public id, so session-start
        // still teaches the mesh CLI with the resolved id as --from.
        let codex = mesh_instructions_for_backend(Some("codex-cli"), "feat/123-worker");
        assert!(codex.contains("ouija ls"), "{codex}");
        assert!(codex.contains("ouija ask"), "{codex}");
        assert!(codex.contains("ouija tell"), "{codex}");
        assert!(codex.contains("ouija reply"), "{codex}");
        assert!(codex.contains("returns after delivery"), "{codex}");
        assert!(codex.contains("do not poll"), "{codex}");
        assert!(
            codex.contains("--from feat/123-worker"),
            "must teach the resolved public id as --from: {codex}"
        );

        // Claude/opencode already carry the skill — their output stays empty.
        assert_eq!(mesh_instructions_for_backend(Some("claude-code"), "x"), "");
        assert_eq!(mesh_instructions_for_backend(Some("opencode"), "x"), "");
        assert_eq!(mesh_instructions_for_backend(None, "x"), "");
    }

    #[tokio::test]
    async fn session_start_onboards_already_registered_codex_session() {
        // A pane-registered Codex session with an authoritative pre-bound
        // thread ID still receives onboarding on an idempotent SessionStart.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "feat/worker".into(),
                pane: Some("%70".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("codex-thread-1".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%70", "/home/user/code/proj")];
        let body = SessionStartBody {
            pane: "%70".into(),
            cwd: "/home/user/code/proj".into(),
            backend_session_id: Some("codex-thread-1".into()),
            backend_identity: None,
            adapter: Some("codex-cli".into()),
            launch_session_id: Some("feat/worker".into()),
            launch_credential: None,
            session_incarnation: None,
        };
        let result = session_start_inner(&state, body).await;
        assert_eq!(result["registered"], "feat/worker");
        let output = result["output"].as_str().unwrap();
        assert!(
            output.contains("ouija ls"),
            "codex must be onboarded: {output}"
        );
        assert!(
            output.contains("--from feat/worker"),
            "must use the authoritative registered id: {output}"
        );
        let proto = state.protocol.read().await;
        let session = proto.sessions.get("feat/worker").unwrap();
        assert_eq!(
            session.metadata.backend_session_id.as_deref(),
            Some("codex-thread-1")
        );
    }

    #[tokio::test]
    async fn old_non_codex_hook_cannot_first_bind_with_current_incarnation() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "claude-worker".into(),
                pane: Some("%71".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await = vec![assistant_pane_with_process(
            "%71",
            "/home/user/code/proj",
            "claude",
        )];
        let body = SessionStartBody {
            pane: "%71".into(),
            cwd: "/home/user/code/proj".into(),
            backend_session_id: Some("claude-session-1".into()),
            backend_identity: None,
            adapter: Some("claude-code".into()),
            launch_session_id: Some("claude-worker".into()),
            launch_credential: None,
            session_incarnation: Some(session_incarnation(&state, "claude-worker").await),
        };
        let result = session_start_inner(&state, body).await;
        assert_eq!(
            result["skipped"],
            "existing pane backend generation proof required"
        );
        let proto = state.protocol.read().await;
        let session = proto.sessions.get("claude-worker").unwrap();
        assert_eq!(session.metadata.backend_session_id, None);
    }

    #[tokio::test]
    async fn session_start_rejects_codex_pane_spoofing_claude_adapter() {
        // Project and pane identity are necessary but not sufficient: a Codex
        // pane must not adopt the empty thread slot of a same-project Claude
        // session by claiming a different adapter. The daemon checks the live
        // pane process rather than trusting the request field.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "claude-worker".into(),
                pane: Some("%71".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%71", "/home/user/code/proj")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%71".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-1".into()),
                backend_identity: None,
                adapter: Some("claude-code".into()),
                launch_session_id: Some("claude-worker".into()),
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane backend generation proof required"
        );
        assert_eq!(result["output"], "");
        assert!(
            state.protocol.read().await.sessions["claude-worker"]
                .metadata
                .backend_session_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn session_start_rejects_non_codex_binding_without_managed_launch_identity() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "claude-worker".into(),
                pane: Some("%75".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await = vec![assistant_pane_with_process(
            "%75",
            "/home/user/code/proj",
            "claude",
        )];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%75".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("claude-session-1".into()),
                backend_identity: None,
                adapter: Some("claude-code".into()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane backend generation proof required"
        );
        assert!(
            state.protocol.read().await.sessions["claude-worker"]
                .metadata
                .backend_session_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn session_start_rejects_tokenless_first_binding_for_backend_unset_pane() {
        // A live same-host pane is corroborating evidence, not ownership
        // authority. Its first hook claim still needs the exact incarnation
        // that the daemon stamped when it auto-registered the pane.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "unknown-worker".into(),
                pane: Some("%74".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%74", "/home/user/code/proj")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%74".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-1".into()),
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane backend generation proof required"
        );
        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["unknown-worker"].metadata;
        assert_eq!(metadata.backend, None);
        assert_eq!(metadata.backend_session_id, None);
    }

    #[tokio::test]
    async fn old_hook_cannot_first_bind_with_replacement_current_incarnation() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "unknown-worker".into(),
                pane: Some("%74".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%74", "/home/user/code/proj")];
        let incarnation = session_incarnation(&state, "unknown-worker").await;

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%74".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-1".into()),
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: Some(incarnation),
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane backend generation proof required"
        );
        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["unknown-worker"].metadata;
        assert_eq!(metadata.backend, None);
        assert_eq!(metadata.backend_session_id, None);
    }

    #[tokio::test]
    async fn session_start_rejects_backend_unset_pane_when_live_adapter_mismatches() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "unknown-worker".into(),
                pane: Some("%74".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await = vec![assistant_pane_with_process(
            "%74",
            "/home/user/code/proj",
            "claude",
        )];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%74".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-1".into()),
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("unknown-worker".into()),
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane backend generation proof required"
        );
        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["unknown-worker"].metadata;
        assert!(metadata.backend.is_none());
        assert!(metadata.backend_session_id.is_none());
    }

    #[tokio::test]
    async fn session_start_does_not_bootstrap_backend_for_remote_pane() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "remote-worker".into(),
                pane: Some("%76".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        state
            .protocol
            .write()
            .await
            .sessions
            .get_mut("remote-worker")
            .expect("registered test session")
            .origin = crate::daemon_protocol::Origin::Remote("peer-daemon".into());
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%76", "/home/user/code/proj")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%76".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-1".into()),
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("remote-worker".into()),
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane backend generation proof required"
        );
        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["remote-worker"].metadata;
        assert!(metadata.backend.is_none());
        assert!(metadata.backend_session_id.is_none());
    }

    #[tokio::test]
    async fn session_start_accepts_existing_matching_backend_session_id() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "feat/worker".into(),
                pane: Some("%72".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("codex-thread-1".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%72", "/home/user/code/proj")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%72".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-1".into()),
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("feat/worker".into()),
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(result["registered"], "feat/worker");
        assert!(
            result["output"]
                .as_str()
                .is_some_and(|output| !output.is_empty())
        );
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions["feat/worker"]
                .metadata
                .backend_session_id
                .as_deref(),
            Some("codex-thread-1")
        );
    }

    #[tokio::test]
    async fn session_start_rejects_missing_credential_for_existing_unbound_codex_pane() {
        // The initial Codex thread ID is accepted only from the daemon-issued,
        // launch-scoped credential. Pane, project, adapter, and launch ID alone
        // are all observable values and must not authorize first binding.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "feat/worker".into(),
                pane: Some("%72".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%72", "/home/user/code/proj")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%72".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-1".into()),
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("feat/worker".into()),
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane backend generation proof required"
        );
        assert_eq!(result["output"], "");
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions["feat/worker"].metadata.backend_session_id, None,
            "an unauthenticated first Codex thread claim must not bind the pane"
        );
    }

    #[tokio::test]
    async fn session_start_binds_first_codex_thread_with_one_time_launch_credential() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "feat/worker".into(),
                pane: Some("%72".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    session_start_credential: Some("launch-secret".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%72", "/home/user/code/proj")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%72".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-1".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "codex-cli".into(),
                    session_id: "codex-thread-1".into(),
                }),
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("feat/worker".into()),
                launch_credential: Some("launch-secret".into()),
                session_incarnation: Some(session_incarnation(&state, "feat/worker").await),
            },
        )
        .await;

        assert_eq!(result["registered"], "feat/worker");
        assert!(
            result["output"]
                .as_str()
                .is_some_and(|output| !output.is_empty())
        );
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions["feat/worker"]
                .metadata
                .backend_session_id
                .as_deref(),
            Some("codex-thread-1")
        );
        assert!(
            proto.sessions["feat/worker"]
                .metadata
                .session_start_credential
                .is_none(),
            "the successful first bind must consume the credential"
        );
        drop(proto);

        let replay = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%72".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-2".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "codex-cli".into(),
                    session_id: "codex-thread-2".into(),
                }),
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("feat/worker".into()),
                launch_credential: Some("launch-secret".into()),
                session_incarnation: Some(session_incarnation(&state, "feat/worker").await),
            },
        )
        .await;
        assert!(
            replay["skipped"].as_str().is_some_and(
                |reason| reason.starts_with("paneless SessionStart backend identity rejected:")
            ),
            "credential replay must be rejected: {replay}"
        );
        assert_eq!(
            state.protocol.read().await.sessions["feat/worker"]
                .metadata
                .backend_session_id
                .as_deref(),
            Some("codex-thread-1")
        );
    }

    #[tokio::test]
    async fn credentialed_managed_claude_session_start_binds_existing_pane() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "feat/claude-worker".into(),
                pane: Some("%73".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    session_start_credential: Some("claude-launch-secret".into()),
                    ..Default::default()
                },
            })
            .await;

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%73".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("claude-session-1".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "claude-code".into(),
                    session_id: "claude-session-1".into(),
                }),
                adapter: Some("claude-code".into()),
                launch_session_id: Some("feat/claude-worker".into()),
                launch_credential: Some("claude-launch-secret".into()),
                session_incarnation: Some(session_incarnation(&state, "feat/claude-worker").await),
            },
        )
        .await;

        assert_eq!(result["registered"], "feat/claude-worker");
        assert_eq!(result["output"], "");
        let protocol = state.protocol.read().await;
        let metadata = &protocol.sessions["feat/claude-worker"].metadata;
        assert_eq!(
            metadata.backend_session_id.as_deref(),
            Some("claude-session-1")
        );
        assert!(
            metadata.session_start_credential.is_none(),
            "the successful Claude bind must consume its one-time credential"
        );
    }

    #[tokio::test]
    async fn session_start_rejects_tokenless_thread_rotation_for_existing_local_pane() {
        // Pane, cwd, and live backend provenance are reusable observations.
        // They must not authorize a fresh thread to replace the exact backend
        // binding owned by the daemon's current session incarnation.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "feat/worker".into(),
                pane: Some("%72".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("codex-thread-1".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%72", "/home/user/code/proj")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%72".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-2".into()),
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane backend generation proof required"
        );
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions["feat/worker"]
                .metadata
                .backend_session_id
                .as_deref(),
            Some("codex-thread-1"),
            "a tokenless hook must not replace the existing thread binding"
        );
    }

    #[tokio::test]
    async fn old_hook_cannot_rebind_with_replacement_current_incarnation() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "feat/worker".into(),
                pane: Some("%72".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("codex-thread-1".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%72", "/home/user/code/proj")];
        let incarnation = session_incarnation(&state, "feat/worker").await;

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%72".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-2".into()),
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: Some(incarnation),
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane backend generation proof required"
        );
        assert_eq!(
            state.protocol.read().await.sessions["feat/worker"]
                .metadata
                .backend_session_id
                .as_deref(),
            Some("codex-thread-1")
        );
    }

    #[tokio::test]
    async fn stale_owner_proof_cannot_bind_same_pane_replacement() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "feat/worker".into(),
                pane: Some("%72".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("codex-thread-old".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        let stale_incarnation = session_incarnation(&state, "feat/worker").await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Remove {
                id: "feat/worker".into(),
                keep_worktree: true,
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "feat/worker".into(),
                pane: Some("%72".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("codex-thread-winner".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%72", "/home/user/code/proj")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%72".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-stale".into()),
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: Some(stale_incarnation),
            },
        )
        .await;

        assert_eq!(result["skipped"], "existing pane incarnation mismatch");
        assert_eq!(
            state.protocol.read().await.sessions["feat/worker"]
                .metadata
                .backend_session_id
                .as_deref(),
            Some("codex-thread-winner")
        );
    }

    #[tokio::test]
    async fn session_start_does_not_rotate_when_live_pane_backend_disagrees() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "feat/worker".into(),
                pane: Some("%72".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("codex-thread-1".into()),
                    project_dir: Some("/home/user/code/proj".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await = vec![assistant_pane_with_process(
            "%72",
            "/home/user/code/proj",
            "claude",
        )];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%72".into(),
                cwd: "/home/user/code/proj".into(),
                backend_session_id: Some("codex-thread-2".into()),
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane backend generation proof required"
        );
        assert_eq!(
            state.protocol.read().await.sessions["feat/worker"]
                .metadata
                .backend_session_id
                .as_deref(),
            Some("codex-thread-1")
        );
    }

    #[tokio::test]
    async fn session_start_replaces_proven_cross_backend_pane_owner() {
        let state = crate::state::AppState::new_for_test();
        let root = tempfile::tempdir().unwrap();
        let old_project = root.path().join("ouija");
        let new_project = root.path().join("hub-fundamentals");
        std::fs::create_dir_all(&old_project).unwrap();
        std::fs::create_dir_all(&new_project).unwrap();
        let old_project = old_project.to_string_lossy().into_owned();
        let new_project = new_project.to_string_lossy().into_owned();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "ouija".into(),
                pane: Some("%3".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("oc-old".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted),
                    project_dir: Some(old_project.clone()),
                    canonical_project_identity: Some(old_project),
                    ..Default::default()
                },
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "hub-fundamentals".into(),
                pane: Some("%718".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    backend_session_id: Some("claude-existing".into()),
                    project_dir: Some(new_project.clone()),
                    canonical_project_identity: Some(new_project.clone()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane_with_process("%3", &new_project, "claude")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%3".into(),
                cwd: new_project,
                backend_session_id: Some("claude-new".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "claude-code".into(),
                    session_id: "claude-new".into(),
                }),
                adapter: Some("claude-code".into()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(result["registered"], "hub-fundamentals-2");
        let protocol = state.protocol.read().await;
        assert!(protocol.dormant_sessions.contains_key("ouija"));
        assert_eq!(
            protocol.sessions["hub-fundamentals-2"].pane.as_deref(),
            Some("%3")
        );
    }

    #[tokio::test]
    async fn session_start_rejects_existing_pane_claim_from_another_project() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "hub-worker".into(),
                pane: Some("%0".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("codex-hub-thread".into()),
                    project_dir: Some("/home/daniel/code/hub".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%0", "/home/daniel/code/hub")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%0".into(),
                cwd: "/home/daniel/code/ouija".into(),
                backend_session_id: Some("codex-ouija-thread".into()),
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("hub-worker".into()),
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(result["output"], "");
        assert_eq!(result["skipped"], "existing pane identity mismatch");
        let proto = state.protocol.read().await;
        let session = proto.sessions.get("hub-worker").unwrap();
        assert_eq!(
            session.metadata.backend_session_id.as_deref(),
            Some("codex-hub-thread"),
            "a mismatched hook must not replace the existing thread binding"
        );
    }

    #[tokio::test]
    async fn session_start_rejects_existing_pane_when_live_path_disagrees() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%73".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    backend_session_id: Some("claude-original".into()),
                    project_dir: Some("/home/daniel/code/ouija".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%73", "/home/daniel/code/hub")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%73".into(),
                cwd: "/home/daniel/code/ouija".into(),
                backend_session_id: Some("claude-replacement".into()),
                backend_identity: None,
                adapter: Some("claude-code".into()),
                launch_session_id: Some("worker".into()),
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(result["output"], "");
        assert_eq!(result["skipped"], "existing pane identity mismatch");
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions["worker"]
                .metadata
                .backend_session_id
                .as_deref(),
            Some("claude-original")
        );
    }

    #[tokio::test]
    async fn session_start_registers_new_session() {
        let state = crate::state::AppState::new_for_test();
        let body = SessionStartBody {
            // Use a pane that cannot resolve in the live tmux server.  This
            // test covers the backend-unknown registration path, and a low
            // pane id can otherwise accidentally detect a real Codex pane.
            pane: "%999999999".into(),
            cwd: "/home/user/code/myproject".into(),
            backend_session_id: None,
            backend_identity: None,
            adapter: None,
            launch_session_id: None,
            launch_credential: None,
            session_incarnation: None,
        };
        let result = session_start_inner(&state, body).await;
        assert_eq!(result["registered"], "myproject");
        assert_eq!(
            result["session_incarnation"],
            state.protocol.read().await.sessions["myproject"]
                .metadata
                .session_incarnation
                .to_string(),
            "manual auto-registration must return its exact daemon-issued incarnation"
        );
        // output is intentionally empty — session-start no longer injects mesh
        // state into the LLM context window.
        assert_eq!(result["output"], "");
    }

    #[tokio::test]
    async fn manual_session_start_registers_new_thread_atomically() {
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![assistant_pane_with_process(
            "%999999998",
            "/home/user/code/myproject",
            "claude",
        )];
        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%999999998".into(),
                cwd: "/home/user/code/myproject".into(),
                backend_session_id: Some("manual-thread".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "claude-code".into(),
                    session_id: "manual-thread".into(),
                }),
                adapter: Some("claude-code".into()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(result["registered"], "myproject");
        let proto = state.protocol.read().await;
        let session = &proto.sessions["myproject"];
        assert_eq!(
            session.metadata.backend_session_id.as_deref(),
            Some("manual-thread")
        );
        assert_eq!(
            result["session_incarnation"],
            session.metadata.session_incarnation.to_string()
        );
    }

    async fn register_stale_canonical_session(
        state: &std::sync::Arc<crate::state::AppState>,
    ) -> crate::daemon_protocol::ResourceOwner {
        state.set_reclaim_test_inspection(crate::tmux::ManagedPaneInspection::Missing);
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "myproject".into(),
                pane: Some("%999999997".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/home/user/code/myproject".into()),
                    canonical_project_identity: Some("/home/user/code/myproject".into()),
                    role: Some("canonical role".into()),
                    prompt: Some("preserve this prompt".into()),
                    backend: Some("claude-code".into()),
                    backend_session_id: Some("continued-thread".into()),
                    ..Default::default()
                },
            })
            .await;
        state.protocol.read().await.sessions["myproject"].owner()
    }

    fn continued_thread_start() -> SessionStartBody {
        SessionStartBody {
            pane: "%999999998".into(),
            cwd: "/home/user/code/myproject".into(),
            backend_session_id: Some("continued-thread".into()),
            backend_identity: Some(crate::backend::BackendSessionIdentity {
                backend: "claude-code".into(),
                session_id: "continued-thread".into(),
            }),
            adapter: Some("claude-code".into()),
            launch_session_id: None,
            launch_credential: None,
            session_incarnation: None,
        }
    }

    #[tokio::test]
    async fn session_start_reclaims_missing_canonical_pane_by_complete_backend_identity() {
        let state = crate::state::AppState::new_for_test();
        let stale_owner = register_stale_canonical_session(&state).await;
        *state.cached_assistant_panes.write().await = vec![assistant_pane_with_process(
            "%999999998",
            "/home/user/code/myproject",
            "claude",
        )];

        let result = session_start_inner(&state, continued_thread_start()).await;

        assert_eq!(result["registered"], "myproject", "result: {result}");
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions.len(),
            1,
            "must not create a suffixed duplicate"
        );
        let canonical = &proto.sessions["myproject"];
        assert_eq!(canonical.pane.as_deref(), Some("%999999998"));
        assert_eq!(canonical.metadata.backend.as_deref(), Some("claude-code"));
        assert_eq!(
            canonical.metadata.backend_session_id.as_deref(),
            Some("continued-thread")
        );
        assert_eq!(canonical.metadata.role.as_deref(), Some("canonical role"));
        assert_eq!(
            canonical.metadata.prompt.as_deref(),
            Some("preserve this prompt")
        );
        assert!(canonical.owner().incarnation > stale_owner.incarnation);
    }

    #[tokio::test]
    async fn session_start_reclaims_canonical_from_scanner_created_metadata_only_duplicate() {
        let state = crate::state::AppState::new_for_test();
        let stale_owner = register_stale_canonical_session(&state).await;
        *state.cached_assistant_panes.write().await = vec![assistant_pane_with_process(
            "%999999998",
            "/home/user/code/myproject",
            "claude",
        )];
        state.scan_and_autoregister_panes().await;
        assert!(
            state
                .protocol
                .read()
                .await
                .sessions
                .contains_key("myproject-2")
        );

        let result = session_start_inner(&state, continued_thread_start()).await;

        assert_eq!(result["registered"], "myproject", "result: {result}");
        let proto = state.protocol.read().await;
        assert_eq!(proto.sessions.len(), 1, "scanner duplicate must be removed");
        assert!(!proto.sessions.contains_key("myproject-2"));
        let canonical = &proto.sessions["myproject"];
        assert_eq!(canonical.pane.as_deref(), Some("%999999998"));
        assert_eq!(canonical.metadata.role.as_deref(), Some("canonical role"));
        assert!(canonical.owner().incarnation > stale_owner.incarnation);
    }

    #[tokio::test]
    async fn resumed_backend_recovers_reaped_public_id_and_lifecycle_metadata() {
        let state = crate::state::AppState::new_for_test();
        let worktree = "/home/daniel/.ouija/worktrees/ouija/rootfix";
        let project = crate::project_identity::resolve_project_identity(worktree).unwrap();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "rootfix".into(),
                pane: Some("%802".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some(project.project_dir.clone()),
                    canonical_project_identity: Some(project.canonical_repository.clone()),
                    role: Some("permanent identity fix".into()),
                    bulletin: Some("preserve identity continuity".into()),
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("019fb5e7-1fd4-7861-bd29-6a4860a3be75".into()),
                    prompt: Some("finish the permanent identity fix".into()),
                    reminder: Some("resume only remaining work".into()),
                    parent_session: Some("ouija".into()),
                    idle_policy: Some(crate::daemon_protocol::IdlePolicy::AskParentWhenDone),
                    fresh_context_after_active_secs: Some(3_600),
                    active_context_accumulated_secs: 120,
                    active_context_segment_started_at: Some(1_753_920_000),
                    ..Default::default()
                },
            })
            .await;
        let reaped_owner = state.protocol.read().await.sessions["rootfix"].owner();
        let dormancy = state
            .dormant_owned(
                reaped_owner.clone(),
                Some("%802".into()),
                1_753_920_030,
                crate::daemon_protocol::DormancySource::Reaped,
            )
            .await;
        assert_eq!(
            dormancy,
            crate::state::DormantOwnedOutcome::Dormant {
                id: "rootfix".into()
            }
        );
        assert!(
            !state.protocol.read().await.sessions.contains_key("rootfix"),
            "the regression requires the original live row to have been reaped"
        );
        let parked = state.protocol.read().await.dormant_sessions["rootfix"].clone();
        assert_eq!(parked.metadata.active_context_accumulated_secs, 150);
        assert_eq!(parked.metadata.active_context_segment_started_at, None);
        *state.cached_assistant_panes.write().await = vec![assistant_pane_with_process(
            "%819",
            &project.project_dir,
            "codex",
        )];
        state.set_dormant_recovery_test_inspection(crate::tmux::ManagedPaneInspection::Unmanaged);

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%819".into(),
                cwd: worktree.into(),
                backend_session_id: Some("019fb5e7-1fd4-7861-bd29-6a4860a3be75".into()),
                backend_identity: Some(crate::backend::BackendSessionIdentity {
                    backend: "codex-cli".into(),
                    session_id: "019fb5e7-1fd4-7861-bd29-6a4860a3be75".into(),
                }),
                adapter: Some("codex-cli".into()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(result["registered"], "rootfix", "result: {result}");
        let protocol = state.protocol.read().await;
        assert_eq!(protocol.sessions.len(), 1);
        let resumed = &protocol.sessions["rootfix"];
        assert_eq!(resumed.pane.as_deref(), Some("%819"));
        assert_eq!(
            resumed.metadata.backend_session_id.as_deref(),
            Some("019fb5e7-1fd4-7861-bd29-6a4860a3be75")
        );
        assert_eq!(
            resumed.metadata.role.as_deref(),
            Some("permanent identity fix")
        );
        assert_eq!(
            resumed.metadata.bulletin.as_deref(),
            Some("preserve identity continuity")
        );
        assert_eq!(
            resumed.metadata.prompt.as_deref(),
            Some("finish the permanent identity fix")
        );
        assert_eq!(
            resumed.metadata.reminder.as_deref(),
            Some("resume only remaining work")
        );
        assert_eq!(resumed.metadata.parent_session.as_deref(), Some("ouija"));
        assert_eq!(
            resumed.metadata.idle_policy,
            Some(crate::daemon_protocol::IdlePolicy::AskParentWhenDone)
        );
        assert_eq!(
            resumed.metadata.fresh_context_after_active_secs,
            Some(3_600)
        );
        assert_eq!(resumed.metadata.active_context_accumulated_secs, 150);
        assert_eq!(resumed.metadata.active_context_segment_started_at, None);
        assert!(
            resumed.owner().incarnation > reaped_owner.incarnation,
            "recovery must allocate a fresh daemon-issued incarnation"
        );
    }

    #[tokio::test]
    async fn session_start_reclaim_rejects_a_positive_live_backend_mismatch() {
        let state = crate::state::AppState::new_for_test();
        register_stale_canonical_session(&state).await;
        *state.cached_assistant_panes.write().await = vec![assistant_pane_with_process(
            "%999999998",
            "/home/user/code/myproject",
            "opencode",
        )];

        let result = session_start_inner(&state, continued_thread_start()).await;

        assert_eq!(
            result["skipped"],
            "session-start backend identity does not match live pane"
        );
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions["myproject"].pane.as_deref(),
            Some("%999999997")
        );
        assert!(!proto.sessions.contains_key("myproject-2"));
    }

    #[tokio::test]
    async fn session_start_current_backend_identity_rejects_a_stale_incarnation() {
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![assistant_pane_with_process(
            "%999999998",
            "/home/user/code/myproject",
            "claude",
        )];
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "myproject".into(),
                pane: Some("%999999998".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/home/user/code/myproject".into()),
                    backend: Some("claude-code".into()),
                    backend_session_id: Some("continued-thread".into()),
                    ..Default::default()
                },
            })
            .await;
        let current = session_incarnation(&state, "myproject").await;
        let mut body = continued_thread_start();
        body.session_incarnation = Some(crate::daemon_protocol::SessionIncarnation(current.0 + 1));

        let result = session_start_inner(&state, body).await;

        assert_eq!(result["skipped"], "existing pane incarnation mismatch");
        assert_eq!(session_incarnation(&state, "myproject").await, current);
    }

    #[tokio::test]
    async fn session_start_refuses_home_cwd() {
        // Regression for #1483: a SessionStart firing while cwd is still $HOME
        // (premature opencode mis-fire) must NOT auto-register a generic
        // basename($HOME)-N session that leaks past task cleanup.
        let state = crate::state::AppState::new_for_test();
        let home = std::env::var("HOME").expect("HOME set in test env");
        let body = SessionStartBody {
            pane: "%51".into(),
            cwd: home,
            backend_session_id: None,
            backend_identity: None,
            adapter: None,
            launch_session_id: None,
            launch_credential: None,
            session_incarnation: None,
        };
        let result = session_start_inner(&state, body).await;
        assert!(
            result.get("skipped").is_some(),
            "home-cwd must be refused, got {result:?}"
        );
        assert!(state.find_session_by_pane("%51").await.is_none());
    }

    #[tokio::test]
    async fn local_backend_pane_attestation_session_start_survives_auto_register_disabled() {
        let project = tempfile::tempdir().unwrap();
        let project_dir = project.path().canonicalize().unwrap();
        let project_dir = project_dir.to_string_lossy().into_owned();
        let identity = crate::backend::BackendSessionIdentity {
            backend: "codex-cli".into(),
            session_id: "thread-attested-disabled".into(),
        };
        let state = crate::state::AppState::new_for_test();
        state.settings.write().await.auto_register = false;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane_with_process("%53", &project_dir, "codex")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%53".into(),
                cwd: project_dir,
                backend_session_id: Some(identity.session_id.clone()),
                backend_identity: Some(identity.clone()),
                adapter: Some(identity.backend.clone()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(result["skipped"], "auto_register disabled");
        assert!(state.protocol.read().await.sessions.is_empty());
        assert!(matches!(
            state.local_backend_pane_attestation(&identity).await,
            Some(crate::state::LocalBackendPaneAttestationState::Unique(_))
        ));
    }

    #[tokio::test]
    async fn local_backend_pane_attestation_session_start_survives_home_guard() {
        let home = std::env::var("HOME").expect("HOME set in test env");
        let identity = crate::backend::BackendSessionIdentity {
            backend: "codex-cli".into(),
            session_id: "thread-attested-home".into(),
        };
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane_with_process("%54", &home, "codex")];

        let result = session_start_inner(
            &state,
            SessionStartBody {
                pane: "%54".into(),
                cwd: home,
                backend_session_id: Some(identity.session_id.clone()),
                backend_identity: Some(identity.clone()),
                adapter: Some(identity.backend.clone()),
                launch_session_id: None,
                launch_credential: None,
                session_incarnation: None,
            },
        )
        .await;

        assert_eq!(result["skipped"], "home cwd (premature session-start)");
        assert!(state.protocol.read().await.sessions.is_empty());
        assert!(matches!(
            state.local_backend_pane_attestation(&identity).await,
            Some(crate::state::LocalBackendPaneAttestationState::Unique(_))
        ));
    }

    #[tokio::test]
    async fn session_start_refuses_home_cwd_trailing_slash() {
        let state = crate::state::AppState::new_for_test();
        let home = std::env::var("HOME").expect("HOME set in test env");
        let body = SessionStartBody {
            pane: "%52".into(),
            cwd: format!("{}/", home.trim_end_matches('/')),
            backend_session_id: None,
            backend_identity: None,
            adapter: None,
            launch_session_id: None,
            launch_credential: None,
            session_incarnation: None,
        };
        let result = session_start_inner(&state, body).await;
        assert!(
            result.get("skipped").is_some(),
            "home-cwd (trailing slash) must be refused, got {result:?}"
        );
        assert!(state.find_session_by_pane("%52").await.is_none());
    }

    #[tokio::test]
    async fn session_start_skips_already_registered() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "existing".into(),
                pane: Some("%50".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/home/user/code/existing".into()),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await =
            vec![assistant_pane("%50", "/home/user/code/existing")];
        let body = SessionStartBody {
            pane: "%50".into(),
            cwd: "/home/user/code/existing".into(),
            backend_session_id: None,
            backend_identity: None,
            adapter: None,
            launch_session_id: None,
            launch_credential: None,
            session_incarnation: None,
        };
        let result = session_start_inner(&state, body).await;
        assert_eq!(result["registered"], "existing");
        // Verify only one session exists
        let proto = state.protocol.read().await;
        let count = proto.sessions.len();
        assert_eq!(count, 1, "should still have exactly 1 session, got {count}");
    }

    #[tokio::test]
    async fn session_start_git_failure_preserves_full_worktree_path() {
        let state = crate::state::AppState::new_for_test();
        let body = SessionStartBody {
            pane: "%50".into(),
            cwd: "/home/user/code/ouija/.ouija/worktrees/feature-x".into(),
            backend_session_id: None,
            backend_identity: None,
            adapter: None,
            launch_session_id: None,
            launch_credential: None,
            session_incarnation: None,
        };
        let result = session_start_inner(&state, body).await;
        assert_eq!(result["registered"], "feature-x");
        let protocol = state.protocol.read().await;
        assert_eq!(
            protocol.sessions["feature-x"]
                .metadata
                .project_dir
                .as_deref(),
            Some("/home/user/code/ouija/.ouija/worktrees/feature-x")
        );
    }

    #[tokio::test]
    async fn post_compact_no_session_returns_ok() {
        let state = crate::state::AppState::new_for_test();
        let body = PaneBody {
            pane: Some("%999".into()),
            backend_session_id: None,
            session_incarnation: None,
        };
        let result = post_compact_inner(&state, body).await;
        assert_eq!(result["ok"], true);
        assert_eq!(result["continuation_injected"], false);
    }

    #[tokio::test]
    async fn post_compact_no_pending_continuation() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "compact-test".into(),
                pane: Some("%88".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let body = PaneBody {
            pane: Some("%88".into()),
            backend_session_id: None,
            session_incarnation: Some(session_incarnation(&state, "compact-test").await),
        };
        let result = post_compact_inner(&state, body).await;
        assert_eq!(result["ok"], true);
        assert_eq!(result["continuation_injected"], false);
    }

    #[tokio::test]
    async fn post_compact_drains_and_clears_continuation() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "drain-test".into(),
                pane: Some("%77".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        // Set a pending continuation via the agent's atomic try-set
        let acquired = state
            .try_set_pending_compact_continuation("drain-test", "Continue working.".into())
            .await;
        assert!(acquired, "slot should be empty for a fresh session");

        // Drain should return the continuation
        let continuation = state.drain_agent_compact_continuation("drain-test").await;
        assert_eq!(continuation.as_deref(), Some("Continue working."));

        // Second drain should return None (one-shot)
        let continuation = state.drain_agent_compact_continuation("drain-test").await;
        assert_eq!(continuation, None);
    }

    #[tokio::test]
    async fn post_compact_does_not_drain_replacement_after_authorization() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%77".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let incarnation = session_incarnation(&state, "worker").await;
        let owner = exact_hook_session_owner(&state, Some("%77"), None, Some(incarnation))
            .await
            .expect("the original hook must authorize");

        replace_session_on_same_pane(&state, "worker", "%77").await;
        assert!(
            state
                .try_set_pending_compact_continuation("worker", "replacement turn".into())
                .await
        );

        assert_eq!(
            state.drain_agent_compact_continuation_owned(&owner).await,
            None,
            "the stale hook must not drain the replacement agent"
        );
        assert_eq!(
            state.drain_agent_compact_continuation("worker").await,
            Some("replacement turn".into()),
            "the replacement continuation must remain available"
        );
    }

    #[tokio::test]
    async fn post_compact_does_not_deliver_after_owner_is_replaced() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%77".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let incarnation = session_incarnation(&state, "worker").await;
        let owner = exact_hook_session_owner(&state, Some("%77"), None, Some(incarnation))
            .await
            .expect("the original hook must authorize");
        assert!(
            state
                .try_set_pending_compact_continuation("worker", "old turn".into())
                .await
        );
        let continuation = state
            .drain_agent_compact_continuation_owned(&owner)
            .await
            .expect("the original owner must drain its continuation");

        replace_session_on_same_pane(&state, "worker", "%77").await;

        let error = crate::tmux::locked_inject_owned(&state, &owner, "%77", &continuation, false)
            .await
            .expect_err("delivery must recheck exact ownership");
        assert!(
            error.to_string().contains("no longer owns pane"),
            "unexpected stale-delivery error: {error}"
        );
    }
}
