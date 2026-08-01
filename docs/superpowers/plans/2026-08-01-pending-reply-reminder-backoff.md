# Pending-Reply Reminder Backoff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent rapid backend failures from causing Ouija to inject the same pending-reply reminder continuously.

**Architecture:** Keep transient reminder-attempt timestamps in the existing per-session actor, keyed by `(sender public ID, message ID)` and measured with Tokio's monotonic clock. Route both the immediate overdue `Stopped` path and the pending-reply portion of `IdleTimeout` through one dispatcher that prunes resolved entries, claims due attempts before delivery, and preserves the two existing message formats.

**Tech Stack:** Rust, Ractor, Tokio monotonic time/test clock, Axum test transport, Cargo.

## Global Constraints

- A specific unanswered pending reply may be reminded at most once per configured `idle_timeout_secs`.
- Record an attempt before delivery so failed injection and fast pre-inference backend failure still back off.
- Cooldown is independent per `(sender public ID, message ID)`.
- Remove actor-local cooldown entries once the corresponding pending reply no longer exists.
- Do not persist cooldown state; actor replacement may produce at most one bounded extra reminder.
- Preserve `ouija ask`, `ouija tell`, progress/completed reply handling, manual reminder clearing IDs, lifecycle policy, message text, and backend behavior.
- Do not change `DaemonState`, protocol events/effects, persistence, or the Stateright model.

---

## File Map

- Modify `Cargo.toml`: enable Tokio's `test-util` feature so focused tests can advance monotonic time without wall-clock sleeps.
- Modify and test `src/session_agent.rs`: own cooldown state, select due pending replies, share the throttled dispatcher between both actor paths, and add regressions beside the existing reminder tests.

### Task 1: Throttle Pending-Reply Reminder Attempts

**Files:**
- Modify: `Cargo.toml:27`
- Modify: `src/session_agent.rs:1-160`
- Modify: `src/session_agent.rs:232-303`
- Modify: `src/session_agent.rs:441-550`
- Modify: `src/session_agent.rs:643-674`
- Test: `src/session_agent.rs:740-930`
- Test: `src/session_agent.rs:1600-1690`

**Interfaces:**
- Consumes: `PendingReplyEntry { msg_id: u64, from: String, last_activity: i64, .. }`, `Settings::idle_timeout_secs: u64`, `SessionAgent::is_current`, and `tmux::locked_inject_owned`.
- Produces: `SessionAgentState::claim_due_pending_reply_reminders(&mut self, all_pending: &[PendingReplyEntry], eligible: &[&PendingReplyEntry], now: tokio::time::Instant, cooldown: std::time::Duration) -> Vec<PendingReplyEntry>`.
- Produces: `SessionAgent::send_pending_reply_reminders(&self, all_pending: &[PendingReplyEntry], eligible: &[&PendingReplyEntry], state: &mut SessionAgentState, cooldown: std::time::Duration, clearing_id: Option<u64>)`.
- State: `SessionAgentState::pending_reply_reminder_attempts: HashMap<(String, u64), tokio::time::Instant>`.
- Formatting contract: `clearing_id == None` emits the current detailed `ouija reply` instruction; `Some(id)` emits the current `Pending reply owed` reminder with that clearing ID.

- [ ] **Step 1: Enable deterministic Tokio time in tests**

Change the existing dependency in `Cargo.toml`:

```toml
tokio = { version = "1", features = ["full", "test-util"] }
```

Run:

```bash
cargo check --tests
```

Expected: PASS; this step changes only test-clock availability, not production behavior.

- [ ] **Step 2: Add the rapid stopped-boundary regression**

Add this helper beside `run_stopped_agent_for_one_idle_timeout`:

```rust
async fn spawn_reminder_test_agent(
    state: Arc<AppState>,
    session_id: &str,
) -> (
    ActorRef<SessionMsg>,
    ractor::concurrency::JoinHandle<()>,
) {
    let owner = state.protocol.read().await.sessions[session_id].owner();
    Actor::spawn(
        None,
        SessionAgent {
            app_state: state,
        },
        SessionAgentArgs {
            owner,
            pane: Some("%99".into()),
        },
    )
    .await
    .expect("spawn failed")
}
```

Add the regression beside `pending_reply_arms_idle_recurrence_without_a_manual_reminder`:

```rust
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
    let _ = ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush repeated stopped");
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
```

- [ ] **Step 3: Run the incident regression and verify it is red**

Run:

```bash
cargo test session_agent::tests::rapid_stopped_boundaries_throttle_one_overdue_pending_reply -- --exact --nocapture
```

Expected: FAIL because the current immediate `Stopped` path injects three reminders instead of one. Do not edit production code until this exact failure is observed.

- [ ] **Step 4: Add deterministic cooldown-selection tests**

Add a test fixture:

```rust
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
```

Add these tests beside the other `SessionAgentState` tests:

```rust
#[tokio::test(start_paused = true)]
async fn pending_reply_cooldown_reopens_only_after_the_full_timeout() {
    let (state, messages, server) =
        opencode_reminder_test_state("cooldown-window", None, None).await;
    state.settings.write().await.idle_timeout_secs = 60;
    state.protocol.write().await.pending_replies.insert(
        "cooldown-window".into(),
        vec![PendingReplyEntry {
            msg_id: 10,
            from: "parent".into(),
            message: "question".into(),
            received_at: Utc::now().timestamp() - 61,
            last_activity: Utc::now().timestamp() - 61,
            in_progress: false,
        }],
    );

    let (actor, handle) = spawn_reminder_test_agent(state, "cooldown-window").await;
    actor.cast(SessionMsg::Stopped).expect("initial stopped");
    let _ = ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush initial stopped");
    assert_eq!(messages.lock().await.len(), 1);

    tokio::time::advance(std::time::Duration::from_secs(59)).await;
    actor.cast(SessionMsg::Stopped).expect("stopped before cooldown");
    let _ = ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush early stopped");
    assert_eq!(messages.lock().await.len(), 1);

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    actor.cast(SessionMsg::Stopped).expect("stopped at cooldown");
    let _ = ractor::call!(actor, SessionMsg::GetPendingReplies).expect("flush due stopped");
    assert_eq!(messages.lock().await.len(), 2);

    actor.stop(None);
    handle.await.expect("actor failed");
    server.abort();
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
    let claimed =
        state.claim_due_pending_reply_reminders(&both, &both_eligible, now, cooldown);
    assert_eq!(
        claimed
            .iter()
            .map(|entry| entry.msg_id)
            .collect::<Vec<_>>(),
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
```

- [ ] **Step 5: Run the selection tests and verify they are red**

Run:

```bash
cargo test session_agent::tests::pending_reply_cooldown_ -- --nocapture
```

Expected: compilation FAIL because `claim_due_pending_reply_reminders` does not exist. This is the second TDD gate.

- [ ] **Step 6: Implement the minimal actor-local cooldown**

At the top of `src/session_agent.rs`, import the map:

```rust
use std::{collections::HashMap, sync::Arc};
```

Add the state field:

```rust
pending_reply_reminder_attempts: HashMap<(String, u64), tokio::time::Instant>,
```

Initialize it in `SessionAgentState::new_with_optional_pane`:

```rust
pending_reply_reminder_attempts: HashMap::new(),
```

Add this method to `impl SessionAgentState`:

```rust
fn claim_due_pending_reply_reminders(
    &mut self,
    all_pending: &[PendingReplyEntry],
    eligible: &[&PendingReplyEntry],
    now: tokio::time::Instant,
    cooldown: std::time::Duration,
) -> Vec<PendingReplyEntry> {
    self.pending_reply_reminder_attempts.retain(|(from, msg_id), _| {
        all_pending
            .iter()
            .any(|entry| entry.from == *from && entry.msg_id == *msg_id)
    });

    eligible
        .iter()
        .filter_map(|entry| {
            let key = (entry.from.clone(), entry.msg_id);
            let cooling_down = self
                .pending_reply_reminder_attempts
                .get(&key)
                .is_some_and(|last_attempt| match now.checked_duration_since(*last_attempt) {
                    Some(elapsed) => elapsed < cooldown,
                    None => true,
                });
            if cooling_down {
                return None;
            }
            self.pending_reply_reminder_attempts.insert(key, now);
            Some((*entry).clone())
        })
        .collect()
}
```

This method must retain only identities found in `all_pending`, use a monotonic `tokio::time::Instant`, treat a clock value earlier than the recorded attempt as still cooling down, and insert before returning the entry for delivery.

- [ ] **Step 7: Replace both pending-reply injection sites with one dispatcher**

Replace `send_reminders` with:

```rust
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
```

In `SessionMsg::Stopped`, change the first protocol read to return the full current pending vector along with `has_reminder`, then derive `has_pending`. This makes the subsequent dispatcher run with an empty `all_pending` slice and prune cooldown state after a reply is completed:

```rust
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
```

Keep the current wall-clock overdue filter over that `pending` vector, then call the dispatcher unconditionally:

```rust
let cooldown = std::time::Duration::from_secs(timeout);
self.send_pending_reply_reminders(
    &pending,
    &overdue,
    state,
    cooldown,
    None,
)
.await;
```

Remove the second protocol read and the `if has_pending` wrapper around overdue selection. The dispatcher must run when both `pending` and `overdue` are empty.

In `SessionMsg::IdleTimeout`, keep the existing conditional `tracing::info!`, replace the existing per-entry injection loop, and call the dispatcher unconditionally so an empty pending vector also prunes resolved identities:

```rust
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
```

Keep the existing `tracing::info!` call when `pending` is non-empty. Do not route the separate manual-reminder body through this cooldown; its clearing-ID behavior is unchanged.

- [ ] **Step 8: Run focused tests and confirm green**

Run:

```bash
cargo test session_agent::tests::rapid_stopped_boundaries_throttle_one_overdue_pending_reply -- --exact --nocapture
cargo test session_agent::tests::pending_reply_cooldown_ -- --nocapture
cargo test session_agent::tests::pending_reply_arms_idle_recurrence_without_a_manual_reminder -- --exact --nocapture
cargo test session_agent::tests::explicit_reminder_injects_the_generated_clearing_id -- --exact --nocapture
cargo test session_agent::tests::lifecycle_only_metadata_does_not_arm_idle_recurrence -- --exact --nocapture
```

Expected: every command PASS. The rapid regression observes one delivery, the cooldown tests pass at 59 and 60 seconds, the existing pending idle reminder preserves its clearing-ID text, and manual/lifecycle-only behavior is unchanged.

- [ ] **Step 9: Run module and repository verification**

Run:

```bash
cargo test session_agent::tests -- --nocapture
cargo check --tests
cargo fmt --all -- --check
cargo clippy --all-targets --all-features
cargo test -- --test-threads=1
git diff --check
```

Expected:

- the complete `session_agent::tests` module passes;
- `cargo check`, formatting, and Clippy pass without new warnings;
- the normal non-ignored test suite passes;
- `git diff --check` prints no errors.

Do not run `cargo test model_check_bfs -- --ignored --nocapture`; this change adds no daemon protocol state or transition.

- [ ] **Step 10: Review the final diff and commit**

Run:

```bash
git diff -- Cargo.toml Cargo.lock src/session_agent.rs
git status --short
git add Cargo.toml Cargo.lock src/session_agent.rs
git commit -m "fix: back off pending reply reminders"
```

Before committing, verify the diff contains only:

- Tokio `test-util` feature/lockfile consequences;
- actor-local cooldown state and selector;
- the shared pending-reply dispatcher;
- the focused tests.

Expected: one implementation commit with no service restart, release, push, protocol/persistence edits, or unrelated files.
