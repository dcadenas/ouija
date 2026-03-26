use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::SharedState;

/// Common request body for pane-identified hooks.
#[derive(Debug, Deserialize)]
pub struct PaneBody {
    pub pane: String,
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
        proto
            .sessions
            .values()
            .find(|s| s.pane.as_deref() == Some(&body.pane))
            .cloned()
    };
    let Some(session) = session else {
        return json!({ "skipped": "no session" });
    };
    // Reject if recently registered (stale SessionEnd hook from pre-restart Claude)
    let age = chrono::Utc::now().timestamp() - session.registered_at;
    if session.registered_at > 0 && age < 5 {
        return json!({ "skipped": format!("recently registered ({}s ago)", age) });
    }
    let id = session.id.clone();
    state
        .apply_and_execute(crate::daemon_protocol::Event::Remove {
            id: id.clone(),
            keep_worktree: false,
        })
        .await;
    // Clear tmux @ouija_id
    let pane = body.pane;
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

async fn hook_stop_inner(
    state: &std::sync::Arc<crate::state::AppState>,
    body: PaneBody,
) -> Value {
    if let Some(id) = state.find_session_by_pane(&body.pane).await {
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
    // Notify agent active
    if let Some(id) = state.find_session_by_pane(&body.pane).await {
        state
            .notify_agent(&id, crate::session_agent::SessionMsg::Active)
            .await;
    }

    // Build current snapshot
    let (current_snapshots, my_session) = {
        let proto = state.protocol.read().await;
        let snaps: Vec<crate::state::SessionSnapshot> = proto
            .sessions
            .values()
            .map(|s| crate::state::SessionSnapshot {
                id: s.id.clone(),
                origin: match &s.origin {
                    crate::daemon_protocol::Origin::Local => "local".into(),
                    crate::daemon_protocol::Origin::Remote(_) => "remote".into(),
                    crate::daemon_protocol::Origin::Human(_) => "human".into(),
                },
                role: s.metadata.role.clone(),
                bulletin: s.metadata.bulletin.clone(),
            })
            .collect();
        let me = proto
            .sessions
            .values()
            .find(|s| s.pane.as_deref() == Some(&body.pane))
            .cloned();
        (snaps, me)
    };

    // Compute diff against per-pane baseline
    let previous = {
        let mut baselines = state.session_diff_baselines.lock().unwrap();
        let prev = baselines.get(&body.pane).cloned().unwrap_or_default();
        baselines.insert(body.pane.clone(), current_snapshots.clone());
        prev
    };

    let prev_ids: std::collections::HashSet<&str> =
        previous.iter().map(|s| s.id.as_str()).collect();
    let curr_ids: std::collections::HashSet<&str> =
        current_snapshots.iter().map(|s| s.id.as_str()).collect();

    let joined: Vec<&crate::state::SessionSnapshot> = current_snapshots
        .iter()
        .filter(|s| !prev_ids.contains(s.id.as_str()))
        .collect();
    let left: Vec<&str> = previous
        .iter()
        .filter(|s| !curr_ids.contains(s.id.as_str()))
        .map(|s| s.id.as_str())
        .collect();
    let updated: Vec<&crate::state::SessionSnapshot> = current_snapshots
        .iter()
        .filter(|s| {
            prev_ids.contains(s.id.as_str())
                && previous.iter().find(|p| p.id == s.id) != Some(s)
        })
        .collect();

    // Stale check — is_stale() is on SessionMeta in daemon_protocol.rs
    let stale = my_session.as_ref().and_then(|s| {
        if s.metadata.is_stale() {
            Some(json!({
                "id": s.id,
                "role": s.metadata.role,
                "bulletin": s.metadata.bulletin,
            }))
        } else {
            None
        }
    });

    // Format output
    let mut output_parts: Vec<String> = Vec::new();

    if let Some(ref stale_info) = stale {
        let id = stale_info["id"].as_str().unwrap_or("");
        let role = stale_info["role"].as_str().unwrap_or("none");
        let bulletin = stale_info["bulletin"].as_str().unwrap_or("");
        if !bulletin.is_empty() {
            output_parts.push(format!(
                "<ouija-status type=\"stale\">Your metadata is stale. Current: role=\"{role}\" | bulletin=\"{bulletin}\". Call session_update(id=\"{id}\", role=\"&lt;what you're doing now&gt;\", bulletin=\"&lt;what you can help with or need&gt;\") if these are outdated.</ouija-status>"
            ));
        } else {
            output_parts.push(format!(
                "<ouija-status type=\"stale\">Your metadata is stale (role: \"{role}\", no bulletin). Call session_update(id=\"{id}\", role=\"&lt;what you're doing now&gt;\", bulletin=\"&lt;what you can help with or need&gt;\") to stay discoverable.</ouija-status>"
            ));
        }
    }

    if !joined.is_empty() {
        let mut lines = vec!["<ouija-status type=\"mesh-update\">joined:".to_string()];
        for s in &joined {
            let mut line = format!("  - {} ({})", s.id, s.origin);
            if let Some(ref r) = s.role {
                line.push_str(&format!(" — {r}"));
            }
            lines.push(line);
        }
        lines.push("</ouija-status>".into());
        output_parts.push(lines.join("\n"));
    }

    if !left.is_empty() {
        output_parts.push(format!(
            "<ouija-status type=\"mesh-update\">left: {}</ouija-status>",
            left.join(",")
        ));
    }

    if !updated.is_empty() {
        let mut lines = vec!["<ouija-status type=\"mesh-update\">updated:".to_string()];
        for s in &updated {
            let prev_s = previous.iter().find(|p| p.id == s.id);
            let mut details = Vec::new();
            if s.role != prev_s.and_then(|p| p.role.as_ref()).cloned() {
                details.push(format!("role: {}", s.role.as_deref().unwrap_or("<cleared>")));
            }
            if s.bulletin != prev_s.and_then(|p| p.bulletin.as_ref()).cloned() {
                details.push(format!(
                    "bulletin: {}",
                    s.bulletin.as_deref().unwrap_or("<cleared>")
                ));
            }
            lines.push(format!("  - {}: {}", s.id, details.join(", ")));
        }
        lines.push("</ouija-status>".into());
        output_parts.push(lines.join("\n"));
    }

    json!({
        "output": output_parts.join("\n"),
        "diff": {
            "joined": joined,
            "left": left,
            "updated": updated,
        },
        "stale": stale,
    })
}

#[derive(Debug, Deserialize)]
pub struct PreToolUseBody {
    pub pane: String,
    pub tool_name: String,
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
    // block_interactive is currently always false (no-op).
    // When wired up, this will check injection marker state on the session.
    let _session_id = state.find_session_by_pane(&body.pane).await;
    let blocked = false;

    if !blocked {
        return json!({ "block": false });
    }
    let message = match body.tool_name.as_str() {
        "AskUserQuestion" => "Interactive prompts are disabled while handling ouija messages.\nDo NOT use AskUserQuestion. Instead, respond in prose with the available\noptions and let the user answer via message. If this question was triggered\nby a message from another session, forward the question to them via\nsession_send and continue when they reply.".to_string(),
        "EnterPlanMode" => "Plan mode is disabled while handling ouija messages.\nDo NOT use EnterPlanMode. Instead, write your plan as a prose message to\nthe user or to the session that requested the task via session_send.\nDescribe your approach, list the steps, and ask for approval in the\nmessage. Proceed when they confirm.".to_string(),
        other => format!("Interactive tool '{other}' is disabled while handling ouija messages.\nCommunicate in prose via session_send instead."),
    };
    json!({ "block": true, "message": message })
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

        let body = PaneBody { pane: "%99".into() };
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
        let body = PaneBody { pane: "%99".into() };
        let result = session_end_inner(&state, body).await;
        assert!(result.get("skipped").is_some());
        // Session still exists
        assert!(state.find_session_by_pane("%99").await.is_some());
    }

    #[tokio::test]
    async fn session_end_no_session() {
        let state = crate::state::AppState::new_for_test();
        let body = PaneBody {
            pane: "%999".into(),
        };
        let result = session_end_inner(&state, body).await;
        assert!(result.get("skipped").is_some());
    }

    #[tokio::test]
    async fn hook_stop_no_session_returns_ok() {
        let state = crate::state::AppState::new_for_test();
        let body = PaneBody { pane: "%999".into() };
        let result = hook_stop_inner(&state, body).await;
        assert_eq!(result, json!({ "ok": true }));
    }

    #[tokio::test]
    async fn prompt_submit_returns_empty_for_unknown_pane() {
        let state = crate::state::AppState::new_for_test();
        let body = PaneBody { pane: "%999".into() };
        let result = prompt_submit_inner(&state, body).await;
        assert_eq!(result["output"], "");
    }

    #[tokio::test]
    async fn prompt_submit_detects_joined_sessions() {
        let state = crate::state::AppState::new_for_test();
        // Register observer pane
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "observer".into(),
                pane: Some("%10".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        // First call: sets baseline
        let _ = prompt_submit_inner(&state, PaneBody { pane: "%10".into() }).await;
        // Add newcomer
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "newcomer".into(),
                pane: Some("%11".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    role: Some("working on newcomer".into()),
                    ..Default::default()
                },
            })
            .await;
        // Second call: should detect newcomer
        let result = prompt_submit_inner(&state, PaneBody { pane: "%10".into() }).await;
        let output = result["output"].as_str().unwrap();
        assert!(output.contains("newcomer"), "output should mention newcomer: {output}");
        assert!(output.contains("joined"), "output should contain 'joined': {output}");
    }

    #[tokio::test]
    async fn pre_tool_use_no_session_allows() {
        let state = crate::state::AppState::new_for_test();
        let body = PreToolUseBody {
            pane: "%999".into(),
            tool_name: "AskUserQuestion".into(),
        };
        let result = pre_tool_use_inner(&state, body).await;
        assert_eq!(result["block"], false);
    }
}
