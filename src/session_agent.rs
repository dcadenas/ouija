use std::sync::Arc;

use chrono::{DateTime, Utc};
use ractor::concurrency::JoinHandle;
use ractor::{Actor, ActorProcessingErr, ActorRef, MessagingErr};

use crate::state::{AppState, PendingReply};

/// Messages the session agent handles.
#[derive(Debug)]
pub enum SessionMsg {
    /// Stop hook fired — reset idle timer.
    Stopped,
    /// User typed (UserPromptSubmit) — cancel idle, mark active.
    Active,
    /// A message was injected into this session's pane.
    MessageDelivered {
        from: String,
        message: String,
        expects_reply: bool,
    },
    /// This session sent a reply to someone.
    ReplySent { to: String },
    /// Query: return current pending replies (RPC).
    GetPendingReplies(ractor::RpcReplyPort<Vec<PendingReply>>),
    /// Clear a specific pending reply by sender name.
    ClearPendingReply { from: String },
    /// Internal: idle timer expired.
    IdleTimeout,
}

/// Per-session behavioral state owned by the agent.
#[allow(dead_code)]
pub struct SessionAgentState {
    pub session_id: String,
    pub pane: String,
    pub idle: bool,
    pub last_stopped_at: Option<DateTime<Utc>>,
    pub last_active_at: Option<DateTime<Utc>>,
    pub pending_replies: Vec<PendingReply>,
    idle_timer: Option<JoinHandle<Result<(), MessagingErr<SessionMsg>>>>,
}

impl SessionAgentState {
    pub fn new(session_id: String, pane: String) -> Self {
        Self {
            session_id,
            pane,
            idle: false,
            last_stopped_at: None,
            last_active_at: None,
            pending_replies: Vec::new(),
            idle_timer: None,
        }
    }

    pub fn add_pending_reply(&mut self, from: &str, message: &str) {
        if !self.pending_replies.iter().any(|p| p.from == from) {
            self.pending_replies.push(PendingReply {
                from: from.to_string(),
                message: message.to_string(),
                received_at: Utc::now(),
                reminded: false,
            });
        }
    }

    pub fn clear_pending_reply(&mut self, from: &str) {
        self.pending_replies.retain(|p| p.from != from);
    }
}

/// The actor struct. Holds a reference to shared app state for reading
/// session metadata and performing tmux injection.
pub struct SessionAgent {
    pub app_state: Arc<AppState>,
}

/// Arguments passed when spawning the agent.
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
                let cutoff = Utc::now() - chrono::Duration::seconds(timeout as i64);
                let overdue: Vec<_> = state
                    .pending_replies
                    .iter()
                    .filter(|p| !p.reminded && p.received_at < cutoff)
                    .map(|p| p.from.clone())
                    .collect();

                if !overdue.is_empty() {
                    let pane = state.pane.clone();
                    let vim_mode = self
                        .app_state
                        .sessions
                        .read()
                        .await
                        .get(&state.session_id)
                        .map(|s| s.metadata.vim_mode)
                        .unwrap_or(false);

                    for from in &overdue {
                        let reminder = format!(
                            "You have an unanswered question from {from} — reply using session_send"
                        );
                        let pane = pane.clone();
                        let lock = self.app_state.pane_lock(&pane);
                        let guard = lock.lock().await;
                        let _ = tokio::task::spawn_blocking(move || {
                            crate::tmux::inject(&pane, &reminder, vim_mode)
                        })
                        .await;
                        drop(guard);
                    }

                    for p in &mut state.pending_replies {
                        if !p.reminded && p.received_at < cutoff {
                            p.reminded = true;
                        }
                    }
                }
            }
            SessionMsg::Active => {
                state.idle = false;
                state.last_active_at = Some(Utc::now());
                if let Some(h) = state.idle_timer.take() {
                    h.abort();
                }
            }
            SessionMsg::MessageDelivered {
                from,
                message,
                expects_reply,
            } => {
                if expects_reply {
                    state.add_pending_reply(&from, &message);
                }
            }
            SessionMsg::ReplySent { to } => {
                state.clear_pending_reply(&to);
            }
            SessionMsg::GetPendingReplies(reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(state.pending_replies.clone());
                }
            }
            SessionMsg::ClearPendingReply { from } => {
                state.clear_pending_reply(&from);
            }
            SessionMsg::IdleTimeout => {
                state.idle_timer = None;
                state.idle = true;
                tracing::debug!(
                    session = %state.session_id,
                    pending = state.pending_replies.len(),
                    "idle timeout fired"
                );

                // Remind about un-replied pending questions
                let unreminded: Vec<_> = state
                    .pending_replies
                    .iter()
                    .filter(|p| !p.reminded)
                    .map(|p| (p.from.clone(), p.message.clone()))
                    .collect();

                if !unreminded.is_empty() {
                    tracing::info!(
                        session = %state.session_id,
                        count = unreminded.len(),
                        "reminding about unanswered pending replies"
                    );
                    let pane = state.pane.clone();
                    let vim_mode = self
                        .app_state
                        .sessions
                        .read()
                        .await
                        .get(&state.session_id)
                        .map(|s| s.metadata.vim_mode)
                        .unwrap_or(false);

                    for (from, _message) in &unreminded {
                        let reminder = format!(
                            "You have an unanswered question from {from} — reply using session_send"
                        );
                        let pane = pane.clone();
                        let lock = self.app_state.pane_lock(&pane);
                        let guard = lock.lock().await;
                        let _ = tokio::task::spawn_blocking(move || {
                            crate::tmux::inject(&pane, &reminder, vim_mode)
                        })
                        .await;
                        drop(guard);
                    }

                    for p in &mut state.pending_replies {
                        if !p.reminded {
                            p.reminded = true;
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ractor::Actor;

    #[test]
    fn agent_state_starts_not_idle() {
        let state = SessionAgentState::new("test-sess".into(), "%1".into());
        assert!(!state.idle);
        assert!(state.pending_replies.is_empty());
    }

    #[test]
    fn agent_state_tracks_pending_replies() {
        let mut state = SessionAgentState::new("test-sess".into(), "%1".into());
        state.add_pending_reply("sender-a", "hello?");
        assert_eq!(state.pending_replies.len(), 1);
        // Duplicate sender is ignored
        state.add_pending_reply("sender-a", "hello again?");
        assert_eq!(state.pending_replies.len(), 1);
        // Different sender is added
        state.add_pending_reply("sender-b", "hey");
        assert_eq!(state.pending_replies.len(), 2);
        // Clear
        state.clear_pending_reply("sender-a");
        assert_eq!(state.pending_replies.len(), 1);
        assert_eq!(state.pending_replies[0].from, "sender-b");
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
}
