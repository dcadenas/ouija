use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Utc};
use ractor::concurrency::JoinHandle;
use ractor::{Actor, ActorProcessingErr, ActorRef, MessagingErr};

use crate::daemon_protocol::{IterationLogEntry, PendingReplyEntry, ResourceOwner};
use crate::state::AppState;

/// Hardcoded stall thresholds.
const HARD_STALL_MULTIPLIER: i64 = 10;
/// Absolute cap for hard stall: 30 minutes.
const HARD_STALL_CAP_SECS: u64 = 1800;

/// Compute average interval between consecutive iteration_log timestamps.
/// Returns None if fewer than 3 entries (insufficient data for stall detection).
pub fn compute_average_loop_interval(log: &[IterationLogEntry]) -> Option<i64> {
    if log.len() < 3 {
        return None;
    }
    let intervals: Vec<i64> = log
        .windows(2)
        .map(|w| w[1].timestamp - w[0].timestamp)
        .collect();
    let sum: i64 = intervals.iter().sum();
    Some(sum / intervals.len() as i64)
}

/// Messages the session agent handles.
#[derive(Debug)]
pub enum SessionMsg {
    /// Stop hook fired — reset idle timer.
    Stopped,
    /// User typed (UserPromptSubmit) — cancel idle, mark active.
    Active,
    /// Query: return current pending replies from DaemonState (RPC).
    GetPendingReplies(ractor::RpcReplyPort<Vec<PendingReplyEntry>>),
    /// Session was renamed — update internal owner only when both sides match.
    Renamed {
        old_owner: ResourceOwner,
        new_owner: ResourceOwner,
    },
    /// Internal: idle timer expired.
    IdleTimeout,
    /// loop_next was called — reset loop stall timer.
    #[allow(dead_code)]
    LoopProgress,
    /// Internal: hard stall timer expired (10x average interval or 30min cap).
    LoopHardStall,
    /// MCP tool called: session acknowledged the reminder.
    ClearReminder { clearing_id: u64 },
    /// Atomically set a pending continuation to inject after compact completes (RPC).
    /// Replies `true` if the slot was acquired, `false` if a continuation is already pending.
    /// Used by the compact endpoint to reject concurrent compact attempts on the same session
    /// so a second caller cannot overwrite the first caller's continuation.
    TrySetPendingCompactContinuation(String, ractor::RpcReplyPort<bool>),
    #[cfg(test)]
    TestTrySetAfterOwnershipCheck(
        String,
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
        ractor::RpcReplyPort<bool>,
    ),
    /// Drain (take + clear) the pending compact continuation (RPC).
    DrainPendingCompactContinuation(ractor::RpcReplyPort<Option<String>>),
    /// Internal: watchdog timer expired (no Active or Stopped within 2x idle_timeout).
    WatchdogTimeout,
}

/// Per-session behavioral state owned by the agent.
pub struct SessionAgentState {
    pub owner: ResourceOwner,
    pub pane: Option<String>,
    pub idle: bool,
    pub last_stopped_at: Option<DateTime<Utc>>,
    pub last_active_at: Option<DateTime<Utc>>,
    idle_timer: Option<JoinHandle<Result<(), MessagingErr<SessionMsg>>>>,
    /// Timer for hard loop stall (10x average interval or 30min cap).
    loop_hard_timer: Option<JoinHandle<Result<(), MessagingErr<SessionMsg>>>>,
    /// True when the session has acknowledged the current reminder via ouija.clear-reminder.
    pub reminder_cleared: bool,
    /// Monotonic counter for clearing_id stamped on each reminder injection.
    next_clearing_id: u64,
    /// Watchdog: fires if no Active or Stopped within 2x idle_timeout.
    /// Catches sessions stuck mid-turn (API errors, crashes).
    watchdog_timer: Option<JoinHandle<Result<(), MessagingErr<SessionMsg>>>>,
    /// One-shot continuation text to inject after compact completes.
    pub pending_compact_continuation: Option<String>,
    pending_reply_reminder_attempts: HashMap<(String, u64), tokio::time::Instant>,
}

impl std::fmt::Debug for SessionAgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAgentState")
            .field("owner", &self.owner)
            .field("pane", &self.pane)
            .field("idle", &self.idle)
            .finish_non_exhaustive()
    }
}

async fn claim_hard_stall_restart(
    app_state: &Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
    pane: &str,
) -> bool {
    match app_state.claim_existing_start(owner).await {
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
        Ok(_) | Err(_) => return false,
    }
    let exact_claim = {
        let proto = app_state.protocol.read().await;
        proto
            .sessions
            .get(&owner.session_id)
            .is_some_and(|session| {
                session.owner() == *owner && session.pane.as_deref() == Some(pane)
            })
            && proto
                .lifecycle_leases
                .get(&owner.session_id)
                .is_some_and(|lease| lease.owner == *owner)
    };
    if !exact_claim {
        let _ = app_state.abort_lifecycle(owner).await;
    }
    exact_claim
}

impl SessionAgentState {
    /// Create initial agent state for a pane-backed session.
    #[cfg(test)]
    pub fn new(owner: ResourceOwner, pane: String) -> Self {
        Self::new_with_optional_pane(owner, Some(pane))
    }

    fn new_with_optional_pane(owner: ResourceOwner, pane: Option<String>) -> Self {
        Self {
            owner,
            pane,
            idle: false,
            last_stopped_at: None,
            last_active_at: None,
            idle_timer: None,
            loop_hard_timer: None,
            reminder_cleared: false,
            next_clearing_id: 0,
            watchdog_timer: None,
            pending_compact_continuation: None,
            pending_reply_reminder_attempts: HashMap::new(),
        }
    }

    fn claim_due_pending_reply_reminders(
        &mut self,
        all_pending: &[PendingReplyEntry],
        eligible: &[&PendingReplyEntry],
        now: tokio::time::Instant,
        cooldown: std::time::Duration,
    ) -> Vec<PendingReplyEntry> {
        self.pending_reply_reminder_attempts
            .retain(|(from, msg_id), _| {
                all_pending
                    .iter()
                    .any(|entry| entry.from == *from && entry.msg_id == *msg_id)
            });

        eligible
            .iter()
            .filter_map(|entry| {
                let key = (entry.from.clone(), entry.msg_id);
                let cooling_down =
                    self.pending_reply_reminder_attempts
                        .get(&key)
                        .is_some_and(|last_attempt| {
                            match now.checked_duration_since(*last_attempt) {
                                Some(elapsed) => elapsed < cooldown,
                                None => true,
                            }
                        });
                if cooling_down {
                    return None;
                }
                self.pending_reply_reminder_attempts.insert(key, now);
                Some((*entry).clone())
            })
            .collect()
    }

    fn owns(&self, protocol: &crate::daemon_protocol::DaemonState) -> bool {
        protocol.session_agent_pane_for_owner(&self.owner) == Some(self.pane.as_deref())
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
    pub owner: ResourceOwner,
    pub pane: Option<String>,
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
        tracing::info!(
            session = %args.owner.session_id,
            incarnation = %args.owner.incarnation,
            "session agent started"
        );
        Ok(SessionAgentState::new_with_optional_pane(
            args.owner, args.pane,
        ))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let SessionMsg::Renamed {
            old_owner,
            new_owner,
        } = &message
        {
            let renamed_is_current = (state.owner == *old_owner || state.owner == *new_owner)
                && self
                    .app_state
                    .protocol
                    .read()
                    .await
                    .session_agent_pane_for_owner(new_owner)
                    == Some(state.pane.as_deref());
            if renamed_is_current {
                tracing::info!(
                    old = %old_owner.session_id,
                    new = %new_owner.session_id,
                    incarnation = %new_owner.incarnation,
                    "session agent renamed"
                );
                state.owner = new_owner.clone();
            } else {
                myself.stop(None);
            }
            return Ok(());
        }

        if !self.refresh_renamed_owner(state).await {
            Self::reject_stale_message(message);
            myself.stop(None);
            return Ok(());
        }

        match message {
            SessionMsg::Stopped => {
                let now = Utc::now();
                state.last_stopped_at = Some(now);
                if let Some(h) = state.idle_timer.take() {
                    h.abort();
                }
                let timeout = self.app_state.settings.read().await.idle_timeout_secs;
                // Reset watchdog
                if let Some(h) = state.watchdog_timer.take() {
                    h.abort();
                }
                // Check if there's a reason to arm the idle timer: pending
                // replies or an explicit, non-empty manual reminder. Lifecycle
                // policy metadata alone does not opt a session into recurring
                // nudges. Without either reason, the idle-check would just
                // create a nudge loop (the session responds to clear it, which
                // triggers Active→Stopped→repeat).
                let (pending, has_reminder) = {
                    let proto = self.app_state.protocol.read().await;
                    let session = state
                        .owns(&proto)
                        .then(|| proto.sessions.get(&state.owner.session_id))
                        .flatten();
                    let pending = if session.is_some() {
                        proto
                            .pending_replies
                            .get(&state.owner.session_id)
                            .cloned()
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let reminder = session.is_some_and(|s| s.metadata.has_active_reminder());
                    (pending, reminder)
                };
                let has_pending = !pending.is_empty();

                if has_pending || has_reminder {
                    state.idle_timer = Some(
                        myself.send_after(std::time::Duration::from_secs(timeout), || {
                            SessionMsg::IdleTimeout
                        }),
                    );
                    // Watchdog at 2x catches sessions stuck mid-turn
                    state.watchdog_timer = Some(
                        myself.send_after(std::time::Duration::from_secs(timeout * 2), || {
                            SessionMsg::WatchdogTimeout
                        }),
                    );
                }

                // Nudge about pending replies older than idle_timeout
                let cutoff = Utc::now().timestamp() - timeout as i64;
                let overdue: Vec<&PendingReplyEntry> = pending
                    .iter()
                    .filter(|p| p.last_activity < cutoff)
                    .collect();
                let cooldown = std::time::Duration::from_secs(timeout);
                self.send_pending_reply_reminders(&pending, &overdue, state, cooldown, None)
                    .await;

                self.app_state
                    .apply_and_execute(crate::daemon_protocol::Event::ActiveContextStopped {
                        owner: state.owner.clone(),
                        at: now.timestamp(),
                    })
                    .await;
            }
            SessionMsg::Active => {
                state.idle = false;
                state.reminder_cleared = false;
                let now = Utc::now();
                state.last_active_at = Some(now);
                if let Some(h) = state.idle_timer.take() {
                    h.abort();
                }
                // Cancel watchdog — it will be re-armed in Stopped only if
                // there is pending work (replies or an explicit reminder).
                if let Some(h) = state.watchdog_timer.take() {
                    h.abort();
                }
                self.app_state
                    .apply_and_execute(crate::daemon_protocol::Event::ActiveContextActive {
                        owner: state.owner.clone(),
                        at: now.timestamp(),
                    })
                    .await;
            }
            SessionMsg::GetPendingReplies(reply) => {
                if !reply.is_closed() {
                    let protocol = self.app_state.protocol.read().await;
                    let pending = if state.owns(&protocol) {
                        protocol
                            .pending_replies
                            .get(&state.owner.session_id)
                            .cloned()
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let _ = reply.send(pending);
                }
            }
            SessionMsg::Renamed { .. } => unreachable!("renames handled before owner validation"),
            SessionMsg::LoopProgress => {
                // Cancel existing stall timer
                if let Some(h) = state.loop_hard_timer.take() {
                    h.abort();
                }

                // Compute average interval from iteration_log
                let avg = {
                    let proto = self.app_state.protocol.read().await;
                    state
                        .owns(&proto)
                        .then(|| proto.sessions.get(&state.owner.session_id))
                        .flatten()
                        .map(|s| compute_average_loop_interval(&s.metadata.iteration_log))
                        .unwrap_or(None)
                };

                // Only activate stall detection with 3+ entries
                if let Some(avg) = avg {
                    let hard_secs = ((avg * HARD_STALL_MULTIPLIER) as u64).min(HARD_STALL_CAP_SECS);

                    state.loop_hard_timer = Some(
                        myself.send_after(std::time::Duration::from_secs(hard_secs), || {
                            SessionMsg::LoopHardStall
                        }),
                    );

                    tracing::debug!(
                        session = %state.owner.session_id,
                        avg_interval = avg,
                        hard_timeout = hard_secs,
                        "loop stall timer set"
                    );
                }
            }
            SessionMsg::LoopHardStall => {
                state.loop_hard_timer = None;
                tracing::warn!(
                    session = %state.owner.session_id,
                    "hard loop stall detected — forcing clean context restart"
                );

                self.handle_hard_stall(state).await;
            }
            SessionMsg::ClearReminder { clearing_id } => {
                if clearing_id == state.next_clearing_id {
                    state.reminder_cleared = true;
                    tracing::debug!(
                        session = %state.owner.session_id,
                        clearing_id,
                        "reminder cleared by session"
                    );
                }
            }
            SessionMsg::TrySetPendingCompactContinuation(text, reply) => {
                // If the caller's RPC was cancelled (timeout, task drop, axum
                // disconnect) before we got here, bail out before mutating —
                // otherwise we would reserve the slot without anyone owning it,
                // and every subsequent compact would 409 until the agent is
                // restarted and the next post-compact hook would drain the
                // orphan into an unrelated turn.
                if !self.try_set_compact_continuation(state, text, reply).await {
                    myself.stop(None);
                }
            }
            #[cfg(test)]
            SessionMsg::TestTrySetAfterOwnershipCheck(text, checked, proceed, reply) => {
                let _ = checked.send(());
                let _ = proceed.await;
                if !self.try_set_compact_continuation(state, text, reply).await {
                    myself.stop(None);
                }
            }
            SessionMsg::DrainPendingCompactContinuation(reply) => {
                let protocol = self.app_state.protocol.read().await;
                let current = state.owns(&protocol);
                if !reply.is_closed() {
                    let value = current
                        .then(|| state.pending_compact_continuation.take())
                        .flatten();
                    let _ = reply.send(value);
                }
                if !current {
                    myself.stop(None);
                }
            }
            SessionMsg::WatchdogTimeout => {
                state.watchdog_timer = None;
                tracing::warn!(
                    session = %state.owner.session_id,
                    "watchdog timeout: no activity for 2x idle_timeout, treating as idle"
                );
                // Trigger idle handling — same as IdleTimeout
                myself.cast(SessionMsg::IdleTimeout)?;
            }
            SessionMsg::IdleTimeout => {
                state.idle_timer = None;
                state.idle = true;

                if state.reminder_cleared {
                    tracing::debug!(
                        session = %state.owner.session_id,
                        "idle timeout fired but reminder was cleared — skipping injection"
                    );
                } else {
                    state.next_clearing_id += 1;
                    let clearing_id = state.next_clearing_id;

                    // Read session metadata in one lock. Gate the reminder
                    // read through has_active_reminder so empty-string /
                    // whitespace-only reminder bodies are treated as absent
                    // here too — a defensive echo of the Stopped-handler gate
                    // above, so this site is safe even if the Stopped check
                    // is ever bypassed (watchdog, future caller).
                    let (reminder, has_lifecycle_policy, vim_mode, pending) = {
                        let proto = self.app_state.protocol.read().await;
                        let session = state
                            .owns(&proto)
                            .then(|| proto.sessions.get(&state.owner.session_id))
                            .flatten();
                        let reminder = session
                            .filter(|s| s.metadata.has_active_reminder())
                            .and_then(|s| {
                                s.metadata
                                    .effective_reminder(&state.owner.session_id, Some(clearing_id))
                            });
                        let has_lifecycle_policy =
                            session.is_some_and(|s| s.metadata.idle_policy.is_some());
                        let vim_mode = session.map(|s| s.metadata.vim_mode).unwrap_or(false);
                        let pending = if session.is_some() {
                            proto
                                .pending_replies
                                .get(&state.owner.session_id)
                                .cloned()
                                .unwrap_or_default()
                        } else {
                            Vec::new()
                        };
                        (reminder, has_lifecycle_policy, vim_mode, pending)
                    };

                    tracing::debug!(
                        session = %state.owner.session_id,
                        clearing_id,
                        pending = pending.len(),
                        has_reminder = reminder.is_some(),
                        "idle timeout fired"
                    );

                    // Inject explicit reminder text if present. Lifecycle
                    // policy metadata is appended to an opted-in reminder, but
                    // does not create a recurring nudge on its own.
                    if let Some(ref reminder_text) = reminder {
                        let reminder_body = if has_lifecycle_policy {
                            reminder_text.clone()
                        } else {
                            format!(
                                "{reminder_text}\n\nIf you have completed all pending work, run: ouija clear-reminder {clearing_id}"
                            )
                        };
                        let wrapped = format!(
                            "<ouija-status type=\"reminder\" clearing_id=\"{clearing_id}\">{reminder_body}</ouija-status>"
                        );
                        if self.is_current(state).await
                            && let Some(pane) = state.pane.as_deref()
                        {
                            let _ = crate::tmux::locked_inject_owned(
                                &self.app_state,
                                &state.owner,
                                pane,
                                &wrapped,
                                vim_mode,
                            )
                            .await;
                        }
                    }

                    // Append pending reply info with per-message format
                    if !pending.is_empty() {
                        tracing::info!(
                            session = %state.owner.session_id,
                            count = pending.len(),
                            "reminding about unanswered pending replies"
                        );
                    }
                    let eligible = pending.iter().collect::<Vec<_>>();
                    self.send_pending_reply_reminders(
                        &pending,
                        &eligible,
                        state,
                        std::time::Duration::from_secs(
                            self.app_state.settings.read().await.idle_timeout_secs,
                        ),
                        Some(clearing_id),
                    )
                    .await;
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
        tracing::info!(
            session = %state.owner.session_id,
            incarnation = %state.owner.incarnation,
            "session agent stopped"
        );
        Ok(())
    }
}

impl SessionAgent {
    /// Keep an actor alive when a protocol rename committed before its queued
    /// `Renamed` control message reached the mailbox. Ordinary messages ahead
    /// of that control message may safely promote the owner through the exact
    /// local alias only when incarnation and pane are unchanged.
    async fn refresh_renamed_owner(&self, state: &mut SessionAgentState) -> bool {
        let protocol = self.app_state.protocol.read().await;
        if state.owns(&protocol) {
            return true;
        }
        let Some(new_id) = protocol.aliases.get(&state.owner.session_id) else {
            return false;
        };
        let Some(session) = protocol.sessions.get(new_id) else {
            return false;
        };
        let new_owner = session.owner();
        if new_owner.incarnation != state.owner.incarnation
            || protocol.session_agent_pane_for_owner(&new_owner) != Some(state.pane.as_deref())
        {
            return false;
        }
        state.owner = session.owner();
        true
    }

    async fn is_current(&self, state: &SessionAgentState) -> bool {
        state.owns(&*self.app_state.protocol.read().await)
    }

    fn reject_stale_message(message: SessionMsg) {
        match message {
            SessionMsg::GetPendingReplies(reply) if !reply.is_closed() => {
                let _ = reply.send(Vec::new());
            }
            SessionMsg::TrySetPendingCompactContinuation(_, reply) if !reply.is_closed() => {
                let _ = reply.send(false);
            }
            #[cfg(test)]
            SessionMsg::TestTrySetAfterOwnershipCheck(_, _, _, reply) if !reply.is_closed() => {
                let _ = reply.send(false);
            }
            SessionMsg::DrainPendingCompactContinuation(reply) if !reply.is_closed() => {
                let _ = reply.send(None);
            }
            _ => {}
        }
    }

    /// Mutate the actor-local compact slot only while a protocol read guard
    /// proves that this exact owner and pane remain current. Holding the guard
    /// through the synchronous mutation and RPC reply excludes replacement on
    /// another Tokio worker.
    async fn try_set_compact_continuation(
        &self,
        state: &mut SessionAgentState,
        text: String,
        reply: ractor::RpcReplyPort<bool>,
    ) -> bool {
        let protocol = self.app_state.protocol.read().await;
        let current = state.owns(&protocol);
        if reply.is_closed() {
            return current;
        }
        let acquired = current && state.pending_compact_continuation.is_none();
        if acquired {
            state.pending_compact_continuation = Some(text);
        }
        let _ = reply.send(acquired);
        current
    }

    async fn send_pending_reply_reminders(
        &self,
        all_pending: &[PendingReplyEntry],
        eligible: &[&PendingReplyEntry],
        state: &mut SessionAgentState,
        cooldown: std::time::Duration,
        clearing_id: Option<u64>,
    ) {
        if !self.is_current(state).await {
            return;
        }
        let Some(pane) = state.pane.clone() else {
            return;
        };
        let due = state.claim_due_pending_reply_reminders(
            all_pending,
            eligible,
            tokio::time::Instant::now(),
            cooldown,
        );
        let vim_mode = self.app_state.protocol.read().await;
        let vim_mode = vim_mode
            .session_agent_pane_for_owner(&state.owner)
            .filter(|claimed| *claimed == Some(pane.as_str()))
            .and_then(|_| vim_mode.sessions.get(&state.owner.session_id))
            .map(|session| session.metadata.vim_mode)
            .unwrap_or(false);

        for entry in due {
            if !self.is_current(state).await {
                break;
            }
            let reminder = if let Some(clearing_id) = clearing_id {
                format!(
                    "<ouija-status type=\"reminder\" clearing_id=\"{clearing_id}\">Pending reply owed: msg #{} from {}</ouija-status>",
                    entry.msg_id, entry.from
                )
            } else {
                format!(
                    "<ouija-status type=\"reminder\">You have an unanswered question from {} (msg {}) — reply using: ouija reply {} {} \"your answer\"</ouija-status>",
                    entry.from, entry.msg_id, entry.from, entry.msg_id
                )
            };
            let _ = crate::tmux::locked_inject_owned(
                &self.app_state,
                &state.owner,
                &pane,
                &reminder,
                vim_mode,
            )
            .await;
        }
    }

    /// Hard stall: force restart with clean context.
    async fn handle_hard_stall(&self, state: &SessionAgentState) {
        let Some(pane) = state.pane.as_deref() else {
            return;
        };
        let meta = {
            let proto = self.app_state.protocol.read().await;
            proto
                .session_agent_pane_for_owner(&state.owner)
                .filter(|claimed| *claimed == Some(pane))
                .and_then(|_| proto.sessions.get(&state.owner.session_id))
                .map(|s| s.metadata.clone())
        };

        let Some(meta) = meta else {
            return;
        };
        let Some(ref prompt) = meta.prompt else {
            return;
        };

        let prompt = prompt.clone();
        let reminder = meta.reminder.clone();
        let app_state = self.app_state.clone();
        let owner = state.owner.clone();
        let pane = pane.to_string();
        let sid = owner.session_id.clone();

        if !claim_hard_stall_restart(&app_state, &owner, &pane).await {
            return;
        }

        tokio::spawn(async move {
            crate::nostr_transport::restart_session_for_start(
                &app_state,
                &owner,
                &sid,
                true,
                None,
                Some(prompt.as_str()),
                None,
                None,
                None,
                None, // model (fallback to prev_metadata.model inside restart)
                None, // effort (fallback to prev_metadata.effort inside restart)
                reminder.as_deref(),
                crate::nostr_transport::ParentSessionOverride::PreservePrevious,
                None, // idle_policy (fallback to prev_metadata.idle_policy)
            )
            .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State as AxumState;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};
    use ractor::Actor;
    use std::sync::Arc as StdArc;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    async fn prompt_async_recorder(
        AxumState(messages): AxumState<StdArc<Mutex<Vec<String>>>>,
        Json(body): Json<serde_json::Value>,
    ) -> StatusCode {
        messages.lock().await.push(
            body["parts"][0]["text"]
                .as_str()
                .expect("prompt text")
                .to_string(),
        );
        StatusCode::NO_CONTENT
    }

    struct BlockedDueDelivery {
        messages: StdArc<Mutex<Vec<String>>>,
        first_delivery_started: StdArc<tokio::sync::Notify>,
        release_first_delivery: StdArc<tokio::sync::Notify>,
        delivery_count: std::sync::atomic::AtomicUsize,
    }

    async fn blocked_first_prompt_async_recorder(
        AxumState(blocked): AxumState<StdArc<BlockedDueDelivery>>,
        Json(body): Json<serde_json::Value>,
    ) -> StatusCode {
        blocked.messages.lock().await.push(
            body["parts"][0]["text"]
                .as_str()
                .expect("prompt text")
                .to_string(),
        );
        if blocked
            .delivery_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            blocked.first_delivery_started.notify_one();
            blocked.release_first_delivery.notified().await;
        }
        StatusCode::NO_CONTENT
    }

    async fn opencode_reminder_test_state(
        session_id: &str,
        reminder: Option<&str>,
        idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
    ) -> (
        Arc<AppState>,
        StdArc<Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let messages = StdArc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route(
                "/session/{session_id}/prompt_async",
                post(prompt_async_recorder),
            )
            .with_state(messages.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let data_dir = tempfile::tempdir().unwrap().keep();
        let state = AppState::new(crate::config::OuijaConfig {
            name: "session-agent-reminder-test".into(),
            npub: "npub1test".into(),
            port: port - 320,
            data_dir: data_dir.clone(),
            config_dir: data_dir,
        });
        state
            .protocol
            .write()
            .await
            .apply(crate::daemon_protocol::Event::Register {
                id: session_id.into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some(format!("{session_id}-backend")),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    reminder: reminder.map(Into::into),
                    idle_policy,
                    ..Default::default()
                },
            });
        state.settings.write().await.idle_timeout_secs = 1;

        (state, messages, server)
    }

    async fn blocked_due_delivery_test_state(
        session_id: &str,
    ) -> (
        Arc<AppState>,
        StdArc<BlockedDueDelivery>,
        tokio::task::JoinHandle<()>,
    ) {
        let blocked = StdArc::new(BlockedDueDelivery {
            messages: StdArc::new(Mutex::new(Vec::new())),
            first_delivery_started: StdArc::new(tokio::sync::Notify::new()),
            release_first_delivery: StdArc::new(tokio::sync::Notify::new()),
            delivery_count: std::sync::atomic::AtomicUsize::new(0),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route(
                "/session/{session_id}/prompt_async",
                post(blocked_first_prompt_async_recorder),
            )
            .with_state(blocked.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let data_dir = tempfile::tempdir().unwrap().keep();
        let state = AppState::new(crate::config::OuijaConfig {
            name: "session-agent-blocked-due-test".into(),
            npub: "npub1test".into(),
            port: port - 320,
            data_dir: data_dir.clone(),
            config_dir: data_dir,
        });
        state
            .protocol
            .write()
            .await
            .apply(crate::daemon_protocol::Event::Register {
                id: session_id.into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some(format!("{session_id}-backend")),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    reminder: Some("continue the task".into()),
                    fresh_context_after_active_secs: Some(60),
                    ..Default::default()
                },
            });
        state.settings.write().await.idle_timeout_secs = 2;

        (state, blocked, server)
    }

    async fn wait_for_message_count(messages: &StdArc<Mutex<Vec<String>>>, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if messages.lock().await.len() >= expected {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("expected delivery must complete");
    }

    async fn run_stopped_agent_for_one_idle_timeout(state: Arc<AppState>, session_id: &str) {
        let agent = SessionAgent {
            app_state: state.clone(),
        };
        let owner = state.protocol.read().await.sessions[session_id].owner();
        let args = SessionAgentArgs {
            owner,
            pane: Some("%99".into()),
        };
        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");

        actor.cast(SessionMsg::Stopped).expect("send");
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        actor.stop(None);
        handle.await.expect("actor failed");
    }

    async fn spawn_reminder_test_agent(
        state: Arc<AppState>,
        session_id: &str,
    ) -> (ActorRef<SessionMsg>, ractor::concurrency::JoinHandle<()>) {
        let owner = state.protocol.read().await.sessions[session_id].owner();
        Actor::spawn(
            None,
            SessionAgent { app_state: state },
            SessionAgentArgs {
                owner,
                pane: Some("%99".into()),
            },
        )
        .await
        .expect("spawn failed")
    }

    async fn register_test_session(
        state: &std::sync::Arc<crate::state::AppState>,
        id: &str,
        pane: &str,
    ) -> ResourceOwner {
        let mut protocol = state.protocol.write().await;
        protocol.apply(crate::daemon_protocol::Event::Register {
            id: id.into(),
            pane: Some(pane.into()),
            metadata: Default::default(),
        });
        protocol.sessions[id].owner()
    }

    fn test_owner(id: &str) -> ResourceOwner {
        ResourceOwner {
            session_id: id.into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(1),
        }
    }

    fn pending_entry(from: &str, msg_id: u64) -> PendingReplyEntry {
        PendingReplyEntry {
            msg_id,
            from: from.into(),
            message: "question".into(),
            received_at: 1,
            last_activity: 1,
            in_progress: false,
        }
    }

    #[test]
    fn agent_state_starts_not_idle() {
        let state = SessionAgentState::new(test_owner("test-sess"), "%1".into());
        assert!(!state.idle);
    }

    #[tokio::test(start_paused = true)]
    async fn pending_reply_cooldown_reopens_only_after_the_full_timeout() {
        let mut state = SessionAgentState::new(test_owner("worker"), "%1".into());
        let pending = vec![pending_entry("parent", 10)];
        let eligible = pending.iter().collect::<Vec<_>>();
        let cooldown = std::time::Duration::from_secs(60);
        let now = tokio::time::Instant::now();

        assert_eq!(
            state
                .claim_due_pending_reply_reminders(&pending, &eligible, now, cooldown)
                .len(),
            1
        );
        tokio::time::advance(std::time::Duration::from_secs(59)).await;
        assert!(
            state
                .claim_due_pending_reply_reminders(
                    &pending,
                    &eligible,
                    tokio::time::Instant::now(),
                    cooldown,
                )
                .is_empty()
        );
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert_eq!(
            state
                .claim_due_pending_reply_reminders(
                    &pending,
                    &eligible,
                    tokio::time::Instant::now(),
                    cooldown,
                )
                .len(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pending_reply_cooldown_is_independent_per_message_identity() {
        let mut state = SessionAgentState::new(test_owner("worker"), "%1".into());
        let first = vec![pending_entry("parent", 10)];
        let first_eligible = first.iter().collect::<Vec<_>>();
        let cooldown = std::time::Duration::from_secs(60);
        let now = tokio::time::Instant::now();

        assert_eq!(
            state
                .claim_due_pending_reply_reminders(&first, &first_eligible, now, cooldown)
                .len(),
            1
        );

        let both = vec![pending_entry("parent", 10), pending_entry("parent", 11)];
        let both_eligible = both.iter().collect::<Vec<_>>();
        let claimed = state.claim_due_pending_reply_reminders(&both, &both_eligible, now, cooldown);
        assert_eq!(
            claimed.iter().map(|entry| entry.msg_id).collect::<Vec<_>>(),
            vec![11]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pending_reply_cooldown_prunes_resolved_message_identity() {
        let mut state = SessionAgentState::new(test_owner("worker"), "%1".into());
        let first = vec![pending_entry("parent", 10)];
        let eligible = first.iter().collect::<Vec<_>>();
        let now = tokio::time::Instant::now();
        let cooldown = std::time::Duration::from_secs(60);

        assert_eq!(
            state
                .claim_due_pending_reply_reminders(&first, &eligible, now, cooldown)
                .len(),
            1
        );
        assert!(
            state
                .claim_due_pending_reply_reminders(&[], &[], now, cooldown)
                .is_empty()
        );

        assert_eq!(
            state
                .claim_due_pending_reply_reminders(&first, &eligible, now, cooldown)
                .len(),
            1,
            "a newly pending message with the same identity is eligible after pruning"
        );
    }

    #[tokio::test]
    async fn agent_becomes_idle_after_stopped() {
        let state = crate::state::AppState::new_for_test();
        let agent = SessionAgent {
            app_state: state.clone(),
        };
        let owner = register_test_session(&state, "test-idle", "%99").await;
        let args = SessionAgentArgs {
            owner,
            pane: Some("%99".into()),
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
        let owner = register_test_session(&state, "test-active", "%99").await;
        let args = SessionAgentArgs {
            owner,
            pane: Some("%99".into()),
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

    #[tokio::test]
    async fn activity_messages_update_the_active_context_policy_at_safe_boundaries() {
        // Break caught: bypassing the protocol accounting events would leave an
        // opted-in session without an active segment, or would keep charging it
        // after its stopped boundary.
        let state = crate::state::AppState::new_for_test();
        let owner = register_test_session(&state, "active-context", "%99").await;
        state
            .protocol
            .write()
            .await
            .sessions
            .get_mut("active-context")
            .expect("registered session")
            .metadata
            .fresh_context_after_active_secs = Some(60);
        let agent = SessionAgent {
            app_state: state.clone(),
        };
        let args = SessionAgentArgs {
            owner,
            pane: Some("%99".into()),
        };
        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");

        actor.cast(SessionMsg::Active).expect("send active");
        let _ = ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush active");
        assert!(
            state.protocol.read().await.sessions["active-context"]
                .metadata
                .active_context_segment_started_at
                .is_some(),
            "Active must open the existing policy's accounting segment"
        );

        actor.cast(SessionMsg::Stopped).expect("send stopped");
        let _ = ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush stopped");
        assert!(
            state.protocol.read().await.sessions["active-context"]
                .metadata
                .active_context_segment_started_at
                .is_none(),
            "Stopped must close the accounting segment at the safe boundary"
        );

        actor.stop(None);
        handle.await.expect("actor failed");
    }

    #[tokio::test]
    #[ignore = "wall-clock timer-ordering regression; run explicitly"]
    async fn active_cancels_replaced_idle_timer_while_due_delivery_blocks() {
        // Break caught: cancelling a timer after slow due delivery leaves an
        // already-expired idle timeout queued ahead of Active.
        let (state, blocked, server) = blocked_due_delivery_test_state("active-cancel").await;
        let owner = state.protocol.read().await.sessions["active-cancel"].owner();
        let agent = SessionAgent {
            app_state: state.clone(),
        };
        let args = SessionAgentArgs {
            owner,
            pane: Some("%99".into()),
        };
        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");

        actor
            .cast(SessionMsg::Stopped)
            .expect("arm initial idle timer");
        let _ = ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush first stop");
        tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
        state
            .protocol
            .write()
            .await
            .sessions
            .get_mut("active-cancel")
            .expect("registered session")
            .metadata
            .active_context_restart_due = true;

        actor.cast(SessionMsg::Stopped).expect("enter due boundary");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            blocked.first_delivery_started.notified(),
        )
        .await
        .expect("due delivery must enter its controlled block");
        tokio::time::sleep(std::time::Duration::from_millis(950)).await;
        actor
            .cast(SessionMsg::Active)
            .expect("queue active cancellation");
        let rpc_actor = actor.clone();
        tokio::time::timeout(std::time::Duration::from_millis(500), async move {
            ractor::call!(rpc_actor, SessionMsg::GetPendingReplies)
        })
        .await
        .expect("Active and its following actor RPC must complete before due delivery releases")
        .expect("actor RPC must succeed");
        let active_started_at = state.protocol.read().await.sessions["active-cancel"]
            .metadata
            .active_context_segment_started_at;
        assert!(
            active_started_at.is_some(),
            "the Active event must apply at its captured boundary while delivery is blocked"
        );
        // Keep the due request blocked past the replacement timer's deadline.
        // A detached delivery lets Active cancel that timer immediately; a
        // synchronous delivery leaves its IdleTimeout queued behind this turn.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let metadata = &state.protocol.read().await.sessions["active-cancel"].metadata;
        assert_eq!(
            metadata.active_context_segment_started_at,
            active_started_at
        );
        assert_eq!(
            metadata.active_context_accumulated_secs, 0,
            "only a captured stopped boundary may close and charge the active segment"
        );
        blocked.release_first_delivery.notify_one();
        let _ = ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush active");

        assert!(
            !blocked
                .messages
                .lock()
                .await
                .iter()
                .any(|message| message.contains("type=\"reminder\"")),
            "the cancelled pre-boundary idle timer must not inject a reminder"
        );
        actor.stop(None);
        handle.await.expect("actor failed");
        server.abort();
    }

    #[tokio::test]
    #[ignore = "wall-clock timer-ordering regression; run explicitly"]
    async fn repeated_stopped_replaces_idle_timer_before_due_delivery_blocks() {
        // Break caught: a queued timeout from the prior stopped boundary must
        // not run before a repeated stopped boundary replaces its timers.
        let (state, blocked, server) = blocked_due_delivery_test_state("repeat-stop").await;
        let owner = state.protocol.read().await.sessions["repeat-stop"].owner();
        let agent = SessionAgent {
            app_state: state.clone(),
        };
        let args = SessionAgentArgs {
            owner,
            pane: Some("%99".into()),
        };
        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");

        actor
            .cast(SessionMsg::Stopped)
            .expect("arm initial idle timer");
        let _ = ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush first stop");
        tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
        state
            .protocol
            .write()
            .await
            .sessions
            .get_mut("repeat-stop")
            .expect("registered session")
            .metadata
            .active_context_restart_due = true;

        actor
            .cast(SessionMsg::Stopped)
            .expect("enter first due boundary");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            blocked.first_delivery_started.notified(),
        )
        .await
        .expect("due delivery must enter its controlled block");
        tokio::time::sleep(std::time::Duration::from_millis(950)).await;
        actor
            .cast(SessionMsg::Stopped)
            .expect("queue repeated stopped boundary");
        actor
            .cast(SessionMsg::Active)
            .expect("queue active after repeated stopped boundary");
        let rpc_actor = actor.clone();
        tokio::time::timeout(std::time::Duration::from_millis(500), async move {
            ractor::call!(rpc_actor, SessionMsg::GetPendingReplies)
        })
        .await
        .expect("repeated Stopped, Active, and their actor RPC must complete before release")
        .expect("actor RPC must succeed");
        let active_started_at = state.protocol.read().await.sessions["repeat-stop"]
            .metadata
            .active_context_segment_started_at;
        assert!(
            active_started_at.is_some(),
            "the post-stop Active boundary must apply before delivery releases"
        );
        // The repeated boundary must replace the first boundary's timer while
        // its due request remains blocked past the repeated timer's deadline.
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        let metadata = &state.protocol.read().await.sessions["repeat-stop"].metadata;
        assert_eq!(
            metadata.active_context_segment_started_at,
            active_started_at
        );
        assert_eq!(
            metadata.active_context_accumulated_secs, 0,
            "only a captured stopped boundary may close and charge the active segment"
        );
        blocked.release_first_delivery.notify_one();
        let _ = ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush repeated stop");
        wait_for_message_count(&blocked.messages, 2).await;

        let messages = blocked.messages.lock().await;
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.contains("Ouija active-context refresh is due"))
                .count(),
            2,
            "every due stopped boundary must still notify"
        );
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("type=\"reminder\"")),
            "the replaced pre-boundary idle timer must not inject a reminder"
        );
        drop(messages);
        actor.stop(None);
        handle.await.expect("actor failed");
        server.abort();
    }

    #[tokio::test]
    async fn due_refresh_notification_contains_a_complete_fresh_continuation_command() {
        // Break caught: a due boundary that omits the concrete continuation
        // command or prompt semantics leaves the next context unable to resume.
        let (state, messages, server) =
            opencode_reminder_test_state("feature-worker", None, None).await;
        let effects = {
            let mut protocol = state.protocol.write().await;
            let metadata = &mut protocol
                .sessions
                .get_mut("feature-worker")
                .expect("registered session")
                .metadata;
            metadata.fresh_context_after_active_secs = Some(5400);
            metadata.active_context_restart_due = true;
            metadata.prompt = Some("finish the active task".into());
            let owner = protocol.sessions["feature-worker"].owner();
            protocol.apply(crate::daemon_protocol::Event::ActiveContextStopped {
                owner: owner.clone(),
                at: 0,
            })
        };

        state.execute_effects(&effects).await;

        wait_for_message_count(&messages, 1).await;
        let messages = messages.lock().await;
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert!(message.contains("feature-worker"));
        assert!(message.contains("1 hour 30 minutes"));
        assert!(message.contains("concise, verified current-work continuation"));
        assert!(message.contains("Verify live state"));
        assert!(message.contains("stored prompt"));
        assert!(message.contains("will be replayed"));
        assert!(message.contains("Confirm it is a durable base prompt"));
        assert!(message.contains("replace it with `--prompt`"));
        assert!(message.contains("<<'OUIJA_CONTINUATION'"));
        assert!(
            message.contains(
                "ouija restart-session 'feature-worker' --fresh --one-shot-file /dev/stdin"
            )
        );
        server.abort();
    }

    #[tokio::test]
    async fn due_refresh_notification_repairs_a_promptless_session_with_a_durable_base() {
        // Break caught: putting all state in a launch-only continuation leaves
        // every later fresh context without a durable base prompt.
        let (state, messages, server) =
            opencode_reminder_test_state("promptless-worker", None, None).await;
        let effects = {
            let mut protocol = state.protocol.write().await;
            let metadata = &mut protocol
                .sessions
                .get_mut("promptless-worker")
                .expect("registered session")
                .metadata;
            metadata.fresh_context_after_active_secs = Some(60);
            metadata.active_context_restart_due = true;
            let owner = protocol.sessions["promptless-worker"].owner();
            protocol.apply(crate::daemon_protocol::Event::ActiveContextStopped {
                owner: owner.clone(),
                at: 0,
            })
        };

        state.execute_effects(&effects).await;

        wait_for_message_count(&messages, 1).await;
        let messages = messages.lock().await;
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("has no stored prompt"));
        assert!(messages[0].contains("compose a concise durable base prompt"));
        assert!(messages[0].contains("durable_prompt=\"$(cat <<'OUIJA_BASE_PROMPT'"));
        assert!(messages[0].contains("--prompt \"$durable_prompt\""));
        assert!(messages[0].contains("keep mutable current work only"));
        server.abort();
    }

    #[tokio::test]
    async fn due_refresh_notification_skips_a_same_pane_replacement() {
        // Break caught: identifying a delayed notification by public ID or pane
        // alone would inject the old session's instruction into its replacement.
        let (state, messages, server) =
            opencode_reminder_test_state("replacement-worker", None, None).await;
        let stale_effects = {
            let mut protocol = state.protocol.write().await;
            let metadata = &mut protocol
                .sessions
                .get_mut("replacement-worker")
                .expect("registered session")
                .metadata;
            metadata.fresh_context_after_active_secs = Some(60);
            metadata.active_context_restart_due = true;
            let owner = protocol.sessions["replacement-worker"].owner();
            protocol.apply(crate::daemon_protocol::Event::ActiveContextStopped {
                owner: owner.clone(),
                at: 0,
            })
        };
        {
            let mut protocol = state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Remove {
                id: "replacement-worker".into(),
                keep_worktree: true,
            });
            protocol.apply(crate::daemon_protocol::Event::Register {
                id: "replacement-worker".into(),
                pane: Some("%99".into()),
                metadata: Default::default(),
            });
        }

        state.execute_effects(&stale_effects).await;

        assert!(
            messages.lock().await.is_empty(),
            "a stale due effect must not reach the same-pane replacement"
        );
        server.abort();
    }

    #[tokio::test]
    async fn replaced_agent_rejects_messages_without_mutating_its_state() {
        let app_state = crate::state::AppState::new_for_test();
        app_state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: Default::default(),
            })
            .await;
        let old_owner = app_state.protocol.read().await.sessions["worker"].owner();
        let agent = SessionAgent {
            app_state: app_state.clone(),
        };
        let args = SessionAgentArgs {
            owner: old_owner.clone(),
            pane: Some("%99".into()),
        };
        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");

        {
            let mut protocol = app_state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Remove {
                id: "worker".into(),
                keep_worktree: true,
            });
            protocol.apply(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: Default::default(),
            });
            assert_ne!(
                protocol.sessions["worker"].metadata.session_incarnation,
                old_owner.incarnation
            );
        }

        let acquired = ractor::call!(
            actor,
            SessionMsg::TrySetPendingCompactContinuation,
            "stale".into()
        )
        .unwrap_or(false);

        assert!(!acquired, "a replaced actor must reject stale RPC work");
        actor.stop(None);
        handle.await.expect("actor failed");
    }

    #[tokio::test]
    async fn compact_slot_rechecks_owner_after_initial_check() {
        let app_state = crate::state::AppState::new_for_test();
        let old_owner = register_test_session(&app_state, "worker", "%99").await;
        let agent = SessionAgent {
            app_state: app_state.clone(),
        };
        let args = SessionAgentArgs {
            owner: old_owner.clone(),
            pane: Some("%99".into()),
        };
        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");
        let (checked_tx, checked_rx) = tokio::sync::oneshot::channel();
        let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
        let call_actor = actor.clone();
        let call = tokio::spawn(async move {
            ractor::call!(
                call_actor,
                SessionMsg::TestTrySetAfterOwnershipCheck,
                "stale".into(),
                checked_tx,
                proceed_rx
            )
            .unwrap_or(false)
        });

        checked_rx
            .await
            .expect("actor must pass the initial ownership check");
        {
            let mut protocol = app_state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Remove {
                id: "worker".into(),
                keep_worktree: true,
            });
            protocol.apply(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: Default::default(),
            });
            assert_ne!(protocol.sessions["worker"].owner(), old_owner);
        }
        proceed_tx.send(()).expect("resume compact-slot RPC");

        assert!(
            !call.await.expect("compact-slot RPC task failed"),
            "a replacement committed after dispatch must exclude the old actor's slot mutation"
        );
        actor.stop(None);
        handle.await.expect("actor failed");
    }

    #[tokio::test]
    async fn message_queued_before_rename_control_promotes_exact_owner() {
        let app_state = crate::state::AppState::new_for_test();
        let old_owner = register_test_session(&app_state, "worker", "%99").await;
        let agent = SessionAgent {
            app_state: app_state.clone(),
        };
        let args = SessionAgentArgs {
            owner: old_owner.clone(),
            pane: Some("%99".into()),
        };
        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");
        let new_owner = {
            let mut protocol = app_state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Rename {
                old_id: "worker".into(),
                new_id: "renamed".into(),
            });
            protocol.sessions["renamed"].owner()
        };

        actor.cast(SessionMsg::Active).expect("queue message");
        actor
            .cast(SessionMsg::Renamed {
                old_owner,
                new_owner,
            })
            .expect("queue rename");
        let acquired = ractor::call!(
            actor,
            SessionMsg::TrySetPendingCompactContinuation,
            "current".into()
        )
        .unwrap_or(false);

        assert!(
            acquired,
            "a message ahead of rename control must not stop the exact renamed actor"
        );
        actor.stop(None);
        handle.await.expect("actor failed");
    }

    #[tokio::test]
    async fn hard_stall_cannot_claim_same_pane_replacement() {
        let state = crate::state::AppState::new_for_test();
        let stale_owner = register_test_session(&state, "worker", "%same").await;
        {
            let mut proto = state.protocol.write().await;
            proto.apply(crate::daemon_protocol::Event::Remove {
                id: "worker".into(),
                keep_worktree: true,
            });
            proto.apply(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%same".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            });
        }

        assert!(
            !claim_hard_stall_restart(&state, &stale_owner, "%same").await,
            "the hard-stall actor must claim its exact incarnation before restart work"
        );
        assert!(
            !state
                .protocol
                .read()
                .await
                .lifecycle_leases
                .contains_key("worker")
        );
    }

    #[test]
    fn session_metadata_recurrence_fields_default() {
        let meta = crate::state::SessionMetadata::default();
        assert!(meta.reminder.is_none());
        assert!(meta.prompt.is_none());
        assert_eq!(meta.iteration, 0);
        assert!(meta.iteration_log.is_empty());
        assert!(meta.last_iteration_at.is_none());
    }

    #[test]
    fn compute_average_interval_needs_3_entries() {
        let log: Vec<crate::daemon_protocol::IterationLogEntry> = vec![
            crate::daemon_protocol::IterationLogEntry {
                iteration: 1,
                message: None,
                timestamp: 100,
            },
            crate::daemon_protocol::IterationLogEntry {
                iteration: 2,
                message: None,
                timestamp: 200,
            },
        ];
        assert!(compute_average_loop_interval(&log).is_none());
    }

    #[test]
    fn compute_average_interval_with_3_entries() {
        let log = vec![
            crate::daemon_protocol::IterationLogEntry {
                iteration: 1,
                message: None,
                timestamp: 100,
            },
            crate::daemon_protocol::IterationLogEntry {
                iteration: 2,
                message: None,
                timestamp: 200,
            },
            crate::daemon_protocol::IterationLogEntry {
                iteration: 3,
                message: None,
                timestamp: 400,
            },
        ];
        // intervals: 100, 200 → average = 150
        assert_eq!(compute_average_loop_interval(&log), Some(150));
    }

    #[test]
    fn compute_average_interval_empty() {
        let log: Vec<crate::daemon_protocol::IterationLogEntry> = vec![];
        assert!(compute_average_loop_interval(&log).is_none());
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
        let owner = state.protocol.read().await.sessions["test-reminder"].owner();
        let args = SessionAgentArgs {
            owner,
            pane: Some("%99".into()),
        };
        state.settings.write().await.idle_timeout_secs = 1;

        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");
        actor.cast(SessionMsg::Stopped).expect("send");
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        assert!(!handle.is_finished());
        actor.stop(None);
        handle.await.expect("actor failed");
    }

    #[tokio::test]
    async fn lifecycle_only_metadata_does_not_arm_idle_recurrence() {
        let (state, messages, server) = opencode_reminder_test_state(
            "lifecycle-only",
            None,
            Some(crate::daemon_protocol::IdlePolicy::KeepOpen),
        )
        .await;

        run_stopped_agent_for_one_idle_timeout(state, "lifecycle-only").await;

        assert!(
            messages.lock().await.is_empty(),
            "lifecycle-only metadata must not inject a recurring idle reminder"
        );
        server.abort();
    }

    #[tokio::test]
    async fn explicit_reminder_injects_the_generated_clearing_id() {
        let (state, messages, server) = opencode_reminder_test_state(
            "manual-reminder",
            Some("resume the assigned task"),
            Some(crate::daemon_protocol::IdlePolicy::KeepOpen),
        )
        .await;

        run_stopped_agent_for_one_idle_timeout(state, "manual-reminder").await;

        let messages = messages.lock().await;
        assert_eq!(messages.len(), 1);
        assert!(messages[0].starts_with("<ouija-status type=\"reminder\" clearing_id=\"1\">"));
        assert!(messages[0].contains("resume the assigned task"));
        assert!(messages[0].contains("ouija clear-reminder 1"));
        assert!(!messages[0].contains("ouija clear-reminder 0"));
        server.abort();
    }

    #[tokio::test]
    async fn pending_reply_arms_idle_recurrence_without_a_manual_reminder() {
        let (state, messages, server) =
            opencode_reminder_test_state("pending-only", None, None).await;
        state.protocol.write().await.pending_replies.insert(
            "pending-only".into(),
            vec![PendingReplyEntry {
                msg_id: 73,
                from: "requester".into(),
                message: "what is the result?".into(),
                received_at: Utc::now().timestamp(),
                last_activity: Utc::now().timestamp(),
                in_progress: false,
            }],
        );

        run_stopped_agent_for_one_idle_timeout(state, "pending-only").await;

        let messages = messages.lock().await;
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            "<ouija-status type=\"reminder\" clearing_id=\"1\">Pending reply owed: msg #73 from requester</ouija-status>"
        );
        server.abort();
    }

    #[tokio::test]
    async fn rapid_stopped_boundaries_throttle_one_overdue_pending_reply() {
        let (state, messages, server) =
            opencode_reminder_test_state("rapid-pending", None, None).await;
        state.protocol.write().await.pending_replies.insert(
            "rapid-pending".into(),
            vec![PendingReplyEntry {
                msg_id: 74,
                from: "requester".into(),
                message: "what failed?".into(),
                received_at: Utc::now().timestamp() - 2,
                last_activity: Utc::now().timestamp() - 2,
                in_progress: false,
            }],
        );

        let (actor, handle) = spawn_reminder_test_agent(state, "rapid-pending").await;
        actor.cast(SessionMsg::Stopped).expect("first stopped");
        let _ = ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush first stopped");
        wait_for_message_count(&messages, 1).await;

        actor.cast(SessionMsg::Stopped).expect("second stopped");
        actor.cast(SessionMsg::Stopped).expect("third stopped");
        let _ =
            ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush repeated stopped");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            messages.lock().await.len(),
            1,
            "the same overdue pending reply must be attempted only once per idle timeout"
        );
        actor.stop(None);
        handle.await.expect("actor failed");
        server.abort();
    }

    #[test]
    fn agent_state_starts_reminder_not_cleared() {
        let state = SessionAgentState::new(test_owner("test-sess"), "%1".into());
        assert!(!state.reminder_cleared);
        assert_eq!(state.next_clearing_id, 0);
    }

    #[tokio::test]
    async fn active_resets_reminder_cleared() {
        let state = crate::state::AppState::new_for_test();
        let agent = SessionAgent {
            app_state: state.clone(),
        };
        let owner = register_test_session(&state, "test-clear", "%99").await;
        let args = SessionAgentArgs {
            owner,
            pane: Some("%99".into()),
        };
        state.settings.write().await.idle_timeout_secs = 60;

        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");

        actor
            .cast(SessionMsg::ClearReminder { clearing_id: 1 })
            .expect("send");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        actor.cast(SessionMsg::Active).expect("send");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(!handle.is_finished());

        actor.stop(None);
        handle.await.expect("actor failed");
    }

    #[tokio::test]
    async fn clear_reminder_wrong_id_ignored() {
        let state = crate::state::AppState::new_for_test();
        let agent = SessionAgent {
            app_state: state.clone(),
        };
        let owner = register_test_session(&state, "test-wrong-id", "%99").await;
        let args = SessionAgentArgs {
            owner,
            pane: Some("%99".into()),
        };
        state.settings.write().await.idle_timeout_secs = 60;

        let (actor, handle) = Actor::spawn(None, agent, args).await.expect("spawn failed");

        // clearing_id 999 doesn't match next_clearing_id (0), should be ignored
        actor
            .cast(SessionMsg::ClearReminder { clearing_id: 999 })
            .expect("send");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(!handle.is_finished());

        actor.stop(None);
        handle.await.expect("actor failed");
    }
}
