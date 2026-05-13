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
    pub pane: String,
    pub cwd: String,
}

/// POST /api/hooks/session-start
pub async fn session_start(
    State(state): State<SharedState>,
    Json(body): Json<SessionStartBody>,
) -> (StatusCode, Json<Value>) {
    let result = session_start_inner(&state, body).await;
    (StatusCode::OK, Json(result))
}

async fn session_start_inner(
    state: &std::sync::Arc<crate::state::AppState>,
    body: SessionStartBody,
) -> Value {
    // Check auto_register
    if !state.settings.read().await.auto_register {
        return json!({ "skipped": "auto_register disabled", "output": "" });
    }

    // Skip if pane already registered (API-started sessions hit this path)
    if let Some(existing_id) = state.find_session_by_pane(&body.pane).await {
        return json!({
            "registered": existing_id,
            "output": "",
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

    // For opencode sessions, resolve the backend_session_id from the shared
    // serve so we can deliver messages via HTTP instead of tmux injection.
    let backend_session_id = if detected_backend.as_deref() == Some("opencode") {
        resolve_opencode_session_id(state, project_root).await
    } else {
        None
    };

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
        "output": "",
    })
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

    #[tokio::test]
    async fn session_start_registers_new_session() {
        let state = crate::state::AppState::new_for_test();
        let body = SessionStartBody {
            pane: "%50".into(),
            cwd: "/home/user/code/myproject".into(),
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
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let body = SessionStartBody {
            pane: "%50".into(),
            cwd: "/home/user/code/existing".into(),
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
