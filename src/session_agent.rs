use std::sync::Arc;

use chrono::{DateTime, Utc};
use ractor::concurrency::JoinHandle;
use ractor::{Actor, ActorProcessingErr, ActorRef, MessagingErr};

use crate::daemon_protocol::PendingReplyEntry;
use crate::state::AppState;

/// Messages the session agent handles.
#[derive(Debug)]
pub enum SessionMsg {
    /// Stop hook fired — reset idle timer.
    Stopped,
    /// User typed (UserPromptSubmit) — cancel idle, mark active.
    Active,
    /// Query: return current pending replies from DaemonState (RPC).
    GetPendingReplies(ractor::RpcReplyPort<Vec<PendingReplyEntry>>),
    /// Session was renamed — update internal session_id.
    Renamed { new_id: String },
    /// Internal: idle timer expired.
    IdleTimeout,
}

/// Per-session behavioral state owned by the agent.
pub struct SessionAgentState {
    pub session_id: String,
    pub pane: String,
    pub idle: bool,
    pub last_stopped_at: Option<DateTime<Utc>>,
    pub last_active_at: Option<DateTime<Utc>>,
    idle_timer: Option<JoinHandle<Result<(), MessagingErr<SessionMsg>>>>,
}

impl std::fmt::Debug for SessionAgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAgentState")
            .field("session_id", &self.session_id)
            .field("pane", &self.pane)
            .field("idle", &self.idle)
            .finish_non_exhaustive()
    }
}

impl SessionAgentState {
    /// Create initial agent state for a session and pane.
    pub fn new(session_id: String, pane: String) -> Self {
        Self {
            session_id,
            pane,
            idle: false,
            last_stopped_at: None,
            last_active_at: None,
            idle_timer: None,
        }
    }
}

/// The actor struct. Holds a reference to shared app state for reading
/// session metadata and performing tmux injection.
#[derive(Debug)]
pub struct SessionAgent {
    pub app_state: Arc<AppState>,
}

/// Arguments passed when spawning the agent.
#[derive(Debug)]
pub struct SessionAgentArgs {
    pub session_id: String,
    pub pane: String,
}

#[ractor::async_trait]
impl Actor for SessionAgent {
    type Msg = SessionMsg;
    type State = SessionAgentState;
    type Arguments = SessionAgentArgs;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        tracing::info!("session agent started: {}", args.session_id);
        Ok(SessionAgentState::new(args.session_id, args.pane))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SessionMsg::Stopped => {
                state.last_stopped_at = Some(Utc::now());
                if let Some(h) = state.idle_timer.take() {
                    h.abort();
                }
                let timeout = self.app_state.settings.read().await.idle_timeout_secs;
                state.idle_timer = Some(
                    myself.send_after(std::time::Duration::from_secs(timeout), || {
                        SessionMsg::IdleTimeout
                    }),
                );

                // Nudge about pending replies older than idle_timeout
                let cutoff = Utc::now().timestamp() - timeout as i64;
                let pending = self
                    .app_state
                    .protocol
                    .read()
                    .await
                    .pending_replies
                    .get(&state.session_id)
                    .cloned()
                    .unwrap_or_default();
                let overdue: Vec<String> = pending
                    .iter()
                    .filter(|p| p.last_activity < cutoff)
                    .map(|p| p.from.clone())
                    .collect();

                if !overdue.is_empty() {
                    self.send_reminders(&overdue, state).await;
                }
            }
            SessionMsg::Active => {
                state.idle = false;
                state.last_active_at = Some(Utc::now());
                if let Some(h) = state.idle_timer.take() {
                    h.abort();
                }
            }
            SessionMsg::GetPendingReplies(reply) => {
                if !reply.is_closed() {
                    let pending = self
                        .app_state
                        .protocol
                        .read()
                        .await
                        .pending_replies
                        .get(&state.session_id)
                        .cloned()
                        .unwrap_or_default();
                    let _ = reply.send(pending);
                }
            }
            SessionMsg::Renamed { new_id } => {
                tracing::info!(
                    old = %state.session_id,
                    new = %new_id,
                    "session agent renamed"
                );
                state.session_id = new_id;
            }
            SessionMsg::IdleTimeout => {
                state.idle_timer = None;
                state.idle = true;

                // Read session metadata in one lock
                let (reminder, vim_mode, pending) = {
                    let proto = self.app_state.protocol.read().await;
                    let session = proto.sessions.get(&state.session_id);
                    let reminder = session.and_then(|s| s.metadata.reminder.clone());
                    let vim_mode = session.map(|s| s.metadata.vim_mode).unwrap_or(false);
                    let pending = proto
                        .pending_replies
                        .get(&state.session_id)
                        .cloned()
                        .unwrap_or_default();
                    (reminder, vim_mode, pending)
                };

                tracing::debug!(
                    session = %state.session_id,
                    pending = pending.len(),
                    has_reminder = reminder.is_some(),
                    "idle timeout fired"
                );

                // Inject reminder text if present (fires even without pending replies)
                if let Some(ref reminder_text) = reminder {
                    let wrapped = format!(
                        "<ouija-status type=\"reminder\">{reminder_text}</ouija-status>"
                    );
                    let _ = crate::tmux::locked_inject(
                        &self.app_state,
                        &state.session_id,
                        &state.pane,
                        &wrapped,
                        vim_mode,
                    )
                    .await;
                }

                // Append pending reply info with per-message format
                if !pending.is_empty() {
                    tracing::info!(
                        session = %state.session_id,
                        count = pending.len(),
                        "reminding about unanswered pending replies"
                    );
                    for p in &pending {
                        let msg = format!(
                            "<ouija-status type=\"reminder\">Pending reply owed: msg #{} from {}</ouija-status>",
                            p.msg_id, p.from
                        );
                        let _ = crate::tmux::locked_inject(
                            &self.app_state,
                            &state.session_id,
                            &state.pane,
                            &msg,
                            vim_mode,
                        )
                        .await;
                    }
                }
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        tracing::info!("session agent stopped: {}", state.session_id);
        Ok(())
    }
}

impl SessionAgent {
    /// Inject pending-reply reminders into the session's pane.
    async fn send_reminders(&self, senders: &[String], state: &SessionAgentState) {
        let vim_mode = self
            .app_state
            .protocol
            .read()
            .await
            .sessions
            .get(&state.session_id)
            .map(|s| s.metadata.vim_mode)
            .unwrap_or(false);

        for from in senders {
            let reminder = format!(
                "<ouija-status type=\"reminder\">You have an unanswered question from {from} — reply using session_send</ouija-status>"
            );
            let _ = crate::tmux::locked_inject(
                &self.app_state,
                &state.session_id,
                &state.pane,
                &reminder,
                vim_mode,
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ractor::Actor;

    #[test]
    fn agent_state_starts_not_idle() {
        let state = SessionAgentState::new("test-sess".into(), "%1".into());
        assert!(!state.idle);
    }

    #[tokio::test]
    async fn agent_becomes_idle_after_stopped() {
        let state = crate::state::AppState::new_for_test();
        let agent = SessionAgent {
            app_state: state.clone(),
        };
        let args = SessionAgentArgs {
            session_id: "test-idle".into(),
            pane: "%99".into(),
        };

        state.settings.write().await.idle_timeout_secs = 1;

        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");

        actor.cast(SessionMsg::Stopped).expect("send failed");
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        assert!(!handle.is_finished());

        actor.stop(None);
        handle.await.expect("actor failed");
    }

    #[tokio::test]
    async fn agent_active_cancels_idle() {
        let state = crate::state::AppState::new_for_test();
        let agent = SessionAgent {
            app_state: state.clone(),
        };
        let args = SessionAgentArgs {
            session_id: "test-active".into(),
            pane: "%99".into(),
        };
        state.settings.write().await.idle_timeout_secs = 1;

        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");

        actor.cast(SessionMsg::Stopped).expect("send");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        actor.cast(SessionMsg::Active).expect("send");
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        assert!(!handle.is_finished());

        actor.stop(None);
        handle.await.expect("actor failed");
    }

    #[test]
    fn session_metadata_loop_fields_default() {
        let meta = crate::state::SessionMetadata::default();
        assert!(meta.reminder.is_none());
        assert!(meta.original_prompt.is_none());
        assert_eq!(meta.loop_iteration, 0);
        assert!(meta.loop_log.is_empty());
    }

    #[tokio::test]
    async fn agent_injects_reminder_on_idle_without_pending_replies() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "test-reminder".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    reminder: Some("call loop_next when done".into()),
                    ..Default::default()
                },
            })
            .await;

        let agent = SessionAgent {
            app_state: state.clone(),
        };
        let args = SessionAgentArgs {
            session_id: "test-reminder".into(),
            pane: "%99".into(),
        };
        state.settings.write().await.idle_timeout_secs = 1;

        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");
        actor.cast(SessionMsg::Stopped).expect("send");
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        assert!(!handle.is_finished());
        actor.stop(None);
        handle.await.expect("actor failed");
    }
}
