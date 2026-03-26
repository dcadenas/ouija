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
}
