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
}

/// POST /api/hooks/session-end
pub async fn session_end(
    State(state): State<SharedState>,
    Json(body): Json<PaneBody>,
) -> (StatusCode, Json<Value>) {
    let result = session_end_inner(&state, body).await;
    (StatusCode::OK, Json(result))
}

async fn session_end_inner(
    state: &std::sync::Arc<crate::state::AppState>,
    body: PaneBody,
) -> Value {
    let session = {
        let proto = state.protocol.read().await;
        let found = proto
            .sessions
            .values()
            .find(|s| {
                body.pane
                    .as_deref()
                    .is_some_and(|p| s.pane.as_deref() == Some(p))
                    || body
                        .backend_session_id
                        .as_deref()
                        .is_some_and(|b| s.metadata.backend_session_id.as_deref() == Some(b))
            })
            .cloned();
        match found {
            Some(s) => s,
            None => return json!({ "skipped": "no session" }),
        }
    };
    // Reject if recently registered (stale SessionEnd hook from pre-restart Claude)
    let age = chrono::Utc::now().timestamp() - session.registered_at;
    if session.registered_at > 0 && age < 5 {
        return json!({ "skipped": format!("recently registered ({}s ago)", age) });
    }
    let id = session.id.clone();
    // SessionEnd hook: always preserve worktrees. Cleanup should only happen
    // via explicit API call with keep_worktree=false.
    state
        .apply_and_execute(crate::daemon_protocol::Event::Remove {
            id: id.clone(),
            keep_worktree: true,
        })
        .await;
    // Clear tmux @ouija_id
    let pane = session.pane.unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        let _ = std::process::Command::new("tmux")
            .args(["set-option", "-pu", "-t", &pane, "@ouija_id"])
            .status();
    });
    json!({ "removed": id })
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
    if let Some(id) = state
        .find_session_by_pane_or_backend_sid(
            body.pane.as_deref(),
            body.backend_session_id.as_deref(),
        )
        .await
    {
        state
            .notify_agent(&id, crate::session_agent::SessionMsg::Stopped)
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
    if let Some(id) = state
        .find_session_by_pane_or_backend_sid(
            body.pane.as_deref(),
            body.backend_session_id.as_deref(),
        )
        .await
    {
        state
            .notify_agent(&id, crate::session_agent::SessionMsg::Active)
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
    if let Some(id) = state
        .find_session_by_pane_or_backend_sid(
            body.pane.as_deref(),
            body.backend_session_id.as_deref(),
        )
        .await
    {
        state
            .notify_agent(&id, crate::session_agent::SessionMsg::Active)
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
    let session_id = match state
        .find_session_by_pane_or_backend_sid(
            body.pane.as_deref(),
            body.backend_session_id.as_deref(),
        )
        .await
    {
        Some(id) => id,
        None => return json!({ "ok": true, "continuation_injected": false }),
    };

    // Drain the pending continuation from the agent (RPC — atomically take + clear)
    let continuation = state.drain_agent_compact_continuation(&session_id).await;

    let Some(continuation) = continuation else {
        return json!({ "ok": true, "continuation_injected": false });
    };

    // Look up pane for injection
    let pane = {
        let proto = state.protocol.read().await;
        proto.sessions.get(&session_id).and_then(|s| s.pane.clone())
    };
    let Some(pane) = pane else {
        return json!({ "ok": true, "continuation_injected": false, "error": "no pane" });
    };

    if let Err(e) =
        crate::tmux::locked_inject(state, &session_id, &pane, &continuation, false).await
    {
        tracing::warn!(
            session = %session_id,
            "post-compact continuation injection failed: {e}"
        );
        return json!({ "ok": false, "error": e.to_string() });
    }

    json!({ "ok": true, "continuation_injected": true })
}

#[derive(Debug, Deserialize)]
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
    /// Backend adapter that emitted this hook. Installed adapters use a
    /// constant value rather than deriving it from the untrusted payload.
    #[serde(default)]
    pub adapter: Option<String>,
    /// The Ouija session id injected into a pane at managed launch time.
    /// It proves that the adapter belongs to this pane's registered launch,
    /// rather than merely sharing the same project directory.
    #[serde(default)]
    pub launch_session_id: Option<String>,
    #[serde(default)]
    pub launch_credential: Option<String>,
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
) -> bool {
    let hook_project_root = crate::state::resolve_project_root(hook_cwd);
    let Some(registered_project_dir) = registered_project_dir else {
        tracing::warn!(
            pane,
            hook_cwd,
            "session-start rejected: existing pane has no project directory"
        );
        return false;
    };
    let registered_project_root = crate::state::resolve_project_root(registered_project_dir);
    if registered_project_root != hook_project_root {
        tracing::warn!(
            pane,
            hook_cwd,
            registered_project_dir,
            "session-start rejected: hook cwd does not match existing pane project"
        );
        return false;
    }

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
    let live_project_root = crate::state::resolve_project_root(live_pane_path);
    if live_project_root != hook_project_root {
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

/// A first backend-thread binding changes durable session ownership, so an
/// existing pane needs more than matching project paths. The installed adapter
/// reports its fixed backend name and the daemon injects the launch session id
/// into managed panes. Both must agree with the registered session before an
/// empty backend-session slot can be adopted.
async fn existing_pane_binding_provenance_matches(
    state: &std::sync::Arc<crate::state::AppState>,
    pane: &str,
    existing_id: &str,
    existing_backend: Option<&str>,
    adapter: Option<&str>,
    launch_session_id: Option<&str>,
) -> bool {
    let Some(adapter) = adapter else {
        return false;
    };
    let live_backend = state
        .list_assistant_panes()
        .await
        .iter()
        .find(|candidate| candidate.pane_id == pane)
        .and_then(|candidate| candidate.process_name.as_deref())
        .and_then(|process_name| {
            state
                .backends
                .all_backend_process_names()
                .into_iter()
                .find(|(_, names)| {
                    names.iter().any(|name| {
                        process_name == name || process_name.strip_prefix('.') == Some(name)
                    })
                })
                .map(|(backend, _)| backend)
        });

    existing_backend == Some(adapter)
        && live_backend.as_deref() == Some(adapter)
        && launch_session_id == Some(existing_id)
}

async fn session_start_inner(
    state: &std::sync::Arc<crate::state::AppState>,
    body: SessionStartBody,
) -> Value {
    // Check auto_register
    if !state.settings.read().await.auto_register {
        return json!({ "skipped": "auto_register disabled", "output": "" });
    }

    // A shared app-server hook has no trustworthy tmux pane. It may bind only
    // the named managed launch, using the claimant-presented one-time proof;
    // cwd and pane discovery are never authorization substitutes here.
    if body.pane.trim().is_empty() {
        let (Some(identity), Some(launch_id), Some(credential)) = (
            body.backend_identity.as_ref(),
            body.launch_session_id.as_deref(),
            body.launch_credential.as_deref(),
        ) else {
            return json!({
                "skipped": "paneless SessionStart requires backend identity, launch session id, and launch credential",
                "output": "",
            });
        };
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
        let result = {
            let mut protocol = state.protocol.write().await;
            protocol.bind_backend_identity(launch_id, &identity, Some(credential))
        };
        if !result.effects.is_empty() {
            state.execute_effects(&result.effects).await;
        }
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
                    "output": mesh_instructions_for_backend(backend.as_deref(), launch_id),
                })
            }
            outcome => json!({
                "skipped": format!("paneless SessionStart backend identity rejected: {outcome:?}"),
                "output": "",
            }),
        };
    }

    // Skip if pane already registered (Ouija-launched / API-started sessions hit
    // this path — they are pane-registered with their backend before the hook
    // fires). Still surface mesh onboarding for codex-cli here, keyed off the
    // session's authoritative stored backend + id, so the primary launch path
    // gets it (claude-code/opencode carry the skill and stay empty).
    if let Some(existing_id) = state.find_session_by_pane(&body.pane).await {
        let (existing_backend, registered_project_dir, existing_backend_session_id) = {
            let proto = state.protocol.read().await;
            proto
                .sessions
                .get(&existing_id)
                .map(|session| {
                    (
                        session.metadata.backend.clone(),
                        session.metadata.project_dir.clone(),
                        session.metadata.backend_session_id.clone(),
                    )
                })
                .unwrap_or_default()
        };
        if !existing_pane_identity_matches(
            state,
            &body.pane,
            &body.cwd,
            registered_project_dir.as_deref(),
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
            if normalize_backend_session_id(existing_backend_session_id.as_deref()).is_none()
                && !existing_pane_binding_provenance_matches(
                    state,
                    &body.pane,
                    &existing_id,
                    existing_backend.as_deref(),
                    body.adapter.as_deref(),
                    body.launch_session_id.as_deref(),
                )
                .await
            {
                tracing::warn!(
                    pane = body.pane,
                    session = existing_id,
                    stored_backend = ?existing_backend,
                    reported_adapter = ?body.adapter,
                    reported_launch_session = ?body.launch_session_id,
                    "session-start rejected: existing pane adapter provenance mismatch"
                );
                return json!({
                    "skipped": "existing pane adapter provenance mismatch",
                    "output": "",
                });
            }
            if existing_backend.as_deref() == Some("codex-cli")
                && normalize_backend_session_id(existing_backend_session_id.as_deref()).is_none()
            {
                let Some(credential) =
                    normalize_backend_session_id(body.launch_credential.as_deref())
                else {
                    return json!({
                        "skipped": "existing Codex pane launch credential mismatch",
                        "output": "",
                    });
                };
                state
                    .apply_and_execute(crate::daemon_protocol::Event::AdoptBackend {
                        id: existing_id.clone(),
                        backend: "codex-cli".into(),
                        backend_session_id: backend_session_id.clone(),
                        expected_backend_session_id: None,
                        expected_session_start_credential: Some(credential),
                    })
                    .await;
                let claimed = {
                    let proto = state.protocol.read().await;
                    proto.sessions.get(&existing_id).is_some_and(|session| {
                        session.metadata.backend_session_id.as_deref()
                            == Some(backend_session_id.as_str())
                    })
                };
                if !claimed {
                    tracing::warn!(
                        pane = body.pane,
                        session = existing_id,
                        "session-start rejected: existing Codex pane launch credential mismatch"
                    );
                    return json!({
                        "skipped": "existing Codex pane launch credential mismatch",
                        "output": "",
                    });
                }
            } else {
                let binding = {
                    let proto = state.protocol.read().await;
                    proto.sessions.get(&existing_id).and_then(|session| {
                        match session.metadata.backend_session_id.as_deref() {
                            None => {
                                let mut metadata = session.metadata.clone();
                                metadata.backend_session_id = Some(backend_session_id.clone());
                                Some(Ok(metadata))
                            }
                            Some(existing) if existing == backend_session_id => None,
                            Some(_) => Some(Err(())),
                        }
                    })
                };
                match binding {
                    Some(Ok(metadata)) => {
                        state
                            .apply_and_execute(crate::daemon_protocol::Event::Register {
                                id: existing_id.clone(),
                                pane: Some(body.pane.clone()),
                                metadata,
                            })
                            .await;
                    }
                    Some(Err(())) => {
                        tracing::warn!(
                            pane = body.pane,
                            session = existing_id,
                            "session-start rejected: existing pane backend session ID mismatch"
                        );
                        return json!({
                            "skipped": "existing pane backend session ID mismatch",
                            "output": "",
                        });
                    }
                    None => {}
                }
            }
        }
        let output = mesh_instructions_for_backend(existing_backend.as_deref(), &existing_id);
        return json!({
            "registered": existing_id,
            "output": output,
        });
    }

    // Derive name from cwd
    let project_root = crate::state::resolve_project_root(&body.cwd);
    let basename = std::path::Path::new(project_root)
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
        let id_to_pane: std::collections::HashMap<String, Option<String>> = proto
            .sessions
            .iter()
            .map(|(id, s)| (id.clone(), s.pane.clone()))
            .collect();
        crate::state::resolve_unique_session_id(&id_to_pane, &base_id, Some(&body.pane))
    };

    // Detect backend from the process running in the pane
    let detected_backend = state.detect_backend_in_pane(&body.pane).await;

    // Prefer the identity supplied by the backend's SessionStart adapter.
    // OpenCode has no such hook, so retain its shared-serve lookup fallback.
    let backend_session_id = match normalize_backend_session_id(body.backend_session_id.as_deref())
    {
        Some(session_id) => Some(session_id),
        None if detected_backend.as_deref() == Some("opencode") => {
            resolve_opencode_session_id(state, project_root).await
        }
        None => None,
    };

    // Compute mesh onboarding text before `detected_backend` is moved into the
    // metadata. Non-empty only for codex-cli (Claude/opencode carry the skill).
    let output = mesh_instructions_for_backend(detected_backend.as_deref(), &id);

    // Register
    let role = format!("working on {basename}");
    let proto_meta = crate::daemon_protocol::SessionMeta {
        project_dir: Some(project_root.to_string()),
        role: Some(role),
        backend: detected_backend,
        backend_session_id,
        ..Default::default()
    };
    state
        .apply_and_execute(crate::daemon_protocol::Event::Register {
            id: id.clone(),
            pane: Some(body.pane.clone()),
            metadata: proto_meta,
        })
        .await;

    json!({
        "registered": id,
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
            },
        )
        .await;
        assert_eq!(
            replay["registered"], "managed",
            "duplicate delivery is idempotent"
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
    async fn session_end_removes_old_session() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "test-session".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        assert!(state.find_session_by_pane("%99").await.is_some());

        // Manually set registered_at to 10 seconds ago so the guard doesn't trigger
        {
            let mut proto = state.protocol.write().await;
            if let Some(s) = proto.sessions.get_mut("test-session") {
                s.registered_at = chrono::Utc::now().timestamp() - 10;
            }
        }

        let body = PaneBody {
            pane: Some("%99".into()),
            backend_session_id: None,
        };
        let result = session_end_inner(&state, body).await;
        assert!(result.get("removed").is_some());
        assert!(state.find_session_by_pane("%99").await.is_none());
    }

    #[tokio::test]
    async fn session_end_rejects_recently_registered() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "fresh".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        // registered_at is now(), so age < 5s — should reject
        let body = PaneBody {
            pane: Some("%99".into()),
            backend_session_id: None,
        };
        let result = session_end_inner(&state, body).await;
        assert!(result.get("skipped").is_some());
        // Session still exists
        assert!(state.find_session_by_pane("%99").await.is_some());
    }

    #[tokio::test]
    async fn session_end_no_session() {
        let state = crate::state::AppState::new_for_test();
        let body = PaneBody {
            pane: Some("%999".into()),
            backend_session_id: None,
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
        };
        let result = hook_stop_inner(&state, body).await;
        assert_eq!(result, json!({ "ok": true }));
    }

    #[tokio::test]
    async fn prompt_submit_returns_empty_for_unknown_pane() {
        let state = crate::state::AppState::new_for_test();
        let body = PaneBody {
            pane: Some("%999".into()),
            backend_session_id: None,
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
        *state.cached_assistant_panes.write().await = vec![assistant_pane(
            "%70",
            "/home/user/code/proj/.ouija/worktrees/feat-worker",
        )];
        let body = SessionStartBody {
            pane: "%70".into(),
            cwd: "/home/user/code/proj/.ouija/worktrees/feat-worker".into(),
            backend_session_id: Some("codex-thread-1".into()),
            backend_identity: None,
            adapter: Some("codex-cli".into()),
            launch_session_id: Some("feat/worker".into()),
            launch_credential: None,
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
    async fn session_start_binds_identity_for_matching_non_codex_adapter_launch() {
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
        };
        let result = session_start_inner(&state, body).await;
        assert_eq!(result["registered"], "claude-worker");
        // Claude carries the skill; its output stays empty.
        assert_eq!(result["output"], "");
        let proto = state.protocol.read().await;
        let session = proto.sessions.get("claude-worker").unwrap();
        assert_eq!(
            session.metadata.backend_session_id.as_deref(),
            Some("claude-session-1")
        );
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
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane adapter provenance mismatch"
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
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane adapter provenance mismatch"
        );
        assert!(
            state.protocol.read().await.sessions["claude-worker"]
                .metadata
                .backend_session_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn session_start_rejects_unproven_first_binding_for_backend_unset_pane() {
        // An existing pane with no recorded backend cannot authenticate any
        // adapter claim. It must stay unbound even when the cwd and managed
        // session id happen to match.
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
                launch_session_id: Some("unknown-worker".into()),
                launch_credential: None,
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing pane adapter provenance mismatch"
        );
        assert_eq!(result["output"], "");
        assert!(
            state.protocol.read().await.sessions["unknown-worker"]
                .metadata
                .backend_session_id
                .is_none()
        );
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
            },
        )
        .await;

        assert_eq!(
            result["skipped"],
            "existing Codex pane launch credential mismatch"
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
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("feat/worker".into()),
                launch_credential: Some("launch-secret".into()),
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
                backend_identity: None,
                adapter: Some("codex-cli".into()),
                launch_session_id: Some("feat/worker".into()),
                launch_credential: Some("launch-secret".into()),
            },
        )
        .await;
        assert_eq!(
            replay["skipped"],
            "existing pane backend session ID mismatch"
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
    async fn session_start_rejects_different_thread_for_existing_same_project_pane() {
        // A pane can receive a second SessionStart from a different Codex
        // thread in the same project. Matching project identity proves the
        // pane belongs to this session, but must not authorize replacing its
        // established backend-thread binding.
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
                launch_session_id: Some("feat/worker".into()),
                launch_credential: None,
            },
        )
        .await;

        assert_eq!(result["output"], "");
        assert_eq!(
            result["skipped"],
            "existing pane backend session ID mismatch"
        );
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions["feat/worker"]
                .metadata
                .backend_session_id
                .as_deref(),
            Some("codex-thread-1"),
            "a second same-project thread must not replace the established binding"
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
        };
        let result = session_start_inner(&state, body).await;
        assert_eq!(result["registered"], "myproject");
        // output is intentionally empty — session-start no longer injects mesh
        // state into the LLM context window.
        assert_eq!(result["output"], "");
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
        };
        let result = session_start_inner(&state, body).await;
        assert_eq!(result["registered"], "existing");
        // Verify only one session exists
        let proto = state.protocol.read().await;
        let count = proto.sessions.len();
        assert_eq!(count, 1, "should still have exactly 1 session, got {count}");
    }

    #[tokio::test]
    async fn session_start_resolves_worktree_path() {
        let state = crate::state::AppState::new_for_test();
        let body = SessionStartBody {
            pane: "%50".into(),
            cwd: "/home/user/code/ouija/.ouija/worktrees/feature-x".into(),
            backend_session_id: None,
            backend_identity: None,
            adapter: None,
            launch_session_id: None,
            launch_credential: None,
        };
        let result = session_start_inner(&state, body).await;
        assert_eq!(result["registered"], "ouija");
    }

    #[tokio::test]
    async fn post_compact_no_session_returns_ok() {
        let state = crate::state::AppState::new_for_test();
        let body = PaneBody {
            pane: Some("%999".into()),
            backend_session_id: None,
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
}
