# Unified Recurring Sessions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unify ouija's scheduled tasks and loop_next into a single recurring session primitive where the session owns runtime recurrence state and the task is a cron trigger with bootstrap config.

**Architecture:** Rename loop_* fields to iteration_* on SessionMeta/SessionMetadata, add on_fire field. Add prompt/reminder bootstrap to ScheduledTask, remove message. Fix NewSession to not kill alive sessions. Update scheduler revival to stamp bootstrap fields on session metadata.

**Tech Stack:** Rust, serde, tokio, ractor (session agent), axum (HTTP API), rmcp (MCP)

**Spec:** `docs/superpowers/specs/2026-03-24-task-loop-unification-design.md`

---

### Task 1: Rename LoopLogEntry to IterationLogEntry in daemon_protocol.rs

**Files:**
- Modify: `src/daemon_protocol.rs:64-71` (struct definition)
- Modify: `src/daemon_protocol.rs:109` (field type reference)
- Modify: `src/daemon_protocol.rs:1593,1608,1614,1620,1626,1634` (tests)
- Modify: `src/session_agent.rs:7` (import)
- Modify: `src/session_agent.rs:536-537,542,554,559,564,576` (test references)
- Modify: `src/state.rs:177` (field type reference)
- Modify: `src/mcp.rs:913` (construction)

- [ ] **Step 1: Rename struct and add type alias for backward compat**

In `src/daemon_protocol.rs`, rename `LoopLogEntry` to `IterationLogEntry` at line 67. Add a type alias so existing code compiles during migration:

```rust
/// A single iteration log entry.
/// Uses i64 timestamp (not DateTime<Utc>) because DaemonState requires Hash+Eq.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct IterationLogEntry {
    pub iteration: u64,
    pub message: Option<String>,
    pub timestamp: i64,
}

/// Backward-compat alias during migration.
pub type LoopLogEntry = IterationLogEntry;
```

- [ ] **Step 2: Update all direct references to use IterationLogEntry**

Replace `LoopLogEntry` with `IterationLogEntry` in:
- `src/daemon_protocol.rs:109` — `pub loop_log: Vec<IterationLogEntry>,`
- `src/session_agent.rs:7` — `use crate::daemon_protocol::{IterationLogEntry, PendingReplyEntry};`
- `src/session_agent.rs:536-537,542,554,559,564,576` — test constructions
- `src/mcp.rs:913` — `let entry = crate::daemon_protocol::IterationLogEntry {`
- `src/state.rs:177` — `pub loop_log: Vec<crate::daemon_protocol::IterationLogEntry>,`
- `src/daemon_protocol.rs:1593,1608,1614,1620,1626,1634` — test constructions

- [ ] **Step 3: Remove the type alias**

Delete the `pub type LoopLogEntry = IterationLogEntry;` line.

- [ ] **Step 4: Run `cargo test` and `cargo clippy`**

Run: `cargo test 2>&1 | tail -5` and `cargo clippy 2>&1 | tail -10`
Expected: all pass, no warnings about LoopLogEntry

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "rename LoopLogEntry to IterationLogEntry"
```

---

### Task 2: Rename session metadata fields in daemon_protocol.rs

**Files:**
- Modify: `src/daemon_protocol.rs:100-112` (SessionMeta field definitions)
- Modify: `src/daemon_protocol.rs:130-146` (inherit_loop_fields_from)
- Modify: `src/daemon_protocol.rs:149-169` (Default impl)
- Modify: `src/daemon_protocol.rs:416-435` (metadata_to_session_meta)
- Modify: `src/daemon_protocol.rs:582-587` (apply_register comment + call)
- Modify: `src/daemon_protocol.rs:1575-1645` (tests)

- [ ] **Step 1: Write test for serde alias backward compat**

Add test in `src/daemon_protocol.rs` tests module:

```rust
#[test]
fn session_meta_serde_aliases_for_renamed_fields() {
    // Old-format JSON with original field names
    let json = r#"{
        "original_prompt": "do work",
        "loop_iteration": 5,
        "loop_log": [{"iteration": 1, "message": null, "timestamp": 100}],
        "last_loop_next": 1711100000
    }"#;
    let meta: SessionMeta = serde_json::from_str(json).unwrap();
    assert_eq!(meta.prompt.as_deref(), Some("do work"));
    assert_eq!(meta.iteration, 5);
    assert_eq!(meta.iteration_log.len(), 1);
    assert_eq!(meta.last_iteration_at, Some(1711100000));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test session_meta_serde_aliases_for_renamed_fields 2>&1 | tail -5`
Expected: FAIL — fields don't exist yet

- [ ] **Step 3: Rename fields on SessionMeta with serde aliases**

In `src/daemon_protocol.rs`, rename the fields (lines 100-112):

```rust
    /// Original prompt from session_start, stored for recurrence.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "original_prompt")]
    pub prompt: Option<String>,
    /// How many times this session has iterated (via loop_next).
    #[serde(default, alias = "loop_iteration")]
    pub iteration: u64,
    /// Log entries from each iteration. Capped at 100.
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "loop_log")]
    pub iteration_log: Vec<IterationLogEntry>,
    /// Unix timestamp of the most recent iteration. Used by stall detection.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "last_loop_next")]
    pub last_iteration_at: Option<i64>,
```

- [ ] **Step 4: Update Default impl** (lines 149-169)

Replace `original_prompt`, `loop_iteration`, `loop_log`, `last_loop_next` with `prompt`, `iteration`, `iteration_log`, `last_iteration_at`.

- [ ] **Step 5: Rename `inherit_loop_fields_from` to `inherit_recurrence_from`** (lines 130-146)

Update method name and field references. Also add `on_fire` inheritance (on_fire field will be added in Task 3):

```rust
    pub fn inherit_recurrence_from(&mut self, source: &SessionMeta) {
        if self.prompt.is_none() {
            self.prompt = source.prompt.clone();
        }
        if self.reminder.is_none() {
            self.reminder = source.reminder.clone();
        }
        if self.iteration == 0 && source.iteration > 0 {
            self.iteration = source.iteration;
        }
        if self.iteration_log.is_empty() && !source.iteration_log.is_empty() {
            self.iteration_log = source.iteration_log.clone();
        }
        if self.last_iteration_at.is_none() && source.last_iteration_at.is_some() {
            self.last_iteration_at = source.last_iteration_at;
        }
    }
```

Note: `on_fire` inheritance is added in Task 3 after the field exists.

- [ ] **Step 6: Update metadata_to_session_meta()** (lines 416-435)

Replace field references: `original_prompt` -> `prompt`, `loop_iteration` -> `iteration`, `loop_log` -> `iteration_log`, `last_loop_next` -> `last_iteration_at`.

- [ ] **Step 7: Update apply_register comment + call** (lines 582-587)

Update comment text and rename `inherit_loop_fields_from` to `inherit_recurrence_from`.

- [ ] **Step 8: Update all tests in daemon_protocol.rs** (lines 1575-1645)

Rename field references in tests: `inherit_loop_fields_carries_last_loop_next`, `loop_log_entry_serde_round_trip`, `loop_log_entry_optional_message`, `loop_log_cap_at_100`, `session_metadata_loop_fields_default`. Update field names and test function names.

- [ ] **Step 9: Run `cargo test` and `cargo clippy`**

Run: `cargo test 2>&1 | tail -5` and `cargo clippy 2>&1 | tail -10`
Expected: FAIL — other files still use old field names (state.rs, mcp.rs, etc.). The daemon_protocol tests should pass.

- [ ] **Step 10: Commit**

```bash
git add -A && git commit -m "rename SessionMeta loop fields to iteration fields with serde aliases"
```

---

### Task 3: Add on_fire to SessionMeta and update inherit_recurrence_from

**Files:**
- Modify: `src/daemon_protocol.rs` (SessionMeta struct, Default impl, inherit_recurrence_from, metadata_to_session_meta)

- [ ] **Step 1: Write test for on_fire inheritance**

```rust
#[test]
fn inherit_recurrence_carries_on_fire() {
    let source = SessionMeta {
        on_fire: Some(crate::scheduler::OnFire::NewSession),
        ..Default::default()
    };
    let mut target = SessionMeta::default();
    target.inherit_recurrence_from(&source);
    assert_eq!(target.on_fire, Some(crate::scheduler::OnFire::NewSession));
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — `on_fire` field doesn't exist on SessionMeta

- [ ] **Step 3: Add on_fire field to SessionMeta**

After the `last_iteration_at` field:

```rust
    /// How this session handles recurrence. None = not a recurring session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_fire: Option<crate::scheduler::OnFire>,
```

Add to Default impl: `on_fire: None,`

Add to `metadata_to_session_meta()`: `on_fire: None,` (SessionMetadata doesn't have it yet, will be added in Task 4).

- [ ] **Step 4: Add on_fire to inherit_recurrence_from**

Add after the reminder check:

```rust
        if self.on_fire.is_none() {
            self.on_fire = source.on_fire.clone();
        }
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test inherit_recurrence_carries_on_fire 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "add on_fire to SessionMeta with inheritance"
```

---

### Task 4: Mirror renames on SessionMetadata in state.rs

**Files:**
- Modify: `src/state.rs:167-181` (field definitions)
- Modify: `src/state.rs:199-205` (Default impl)
- Modify: `src/state.rs:630-634` (protocol conversion)

- [ ] **Step 1: Rename fields on SessionMetadata**

Rename with serde aliases (same pattern as SessionMeta):
- `original_prompt` -> `prompt` with `alias = "original_prompt"`
- `loop_iteration` -> `iteration` with `alias = "loop_iteration"`
- `loop_log` -> `iteration_log` with `alias = "loop_log"`
- `last_loop_next` -> `last_iteration_at` with `alias = "last_loop_next"`

Add new field:

```rust
    /// How this session handles recurrence. None = not a recurring session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_fire: Option<crate::scheduler::OnFire>,
```

- [ ] **Step 2: Update Default impl and protocol conversion**

Update field names in Default impl and in the `state.rs:630-634` protocol conversion.

Update `metadata_to_session_meta()` in daemon_protocol.rs to map the new `on_fire` field:
`on_fire: m.on_fire.clone(),` (replacing the `on_fire: None,` from Task 3).

- [ ] **Step 3: Run `cargo check`**

Run: `cargo check 2>&1 | tail -20`
Expected: FAIL — many consumers still use old field names (mcp.rs, session_agent.rs, etc.)

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "mirror field renames on SessionMetadata, add on_fire"
```

---

### Task 5: Update all consumers of renamed fields

**Files:**
- Modify: `src/mcp.rs:899,907,911-922,941,983` (loop_next impl)
- Modify: `src/session_agent.rs:204,425,455,528-531` (stall detection + tests)
- Modify: `src/nostr_transport.rs:1455,1819-1825,1833` (start/restart_session)
- Modify: `src/api.rs:103-106` (status JSON)
- Modify: `src/admin.rs:70-71` (dashboard)

- [ ] **Step 1: Update mcp.rs loop_next**

- Line 899: `meta.original_prompt` -> `meta.prompt`
- Line 907: `meta.loop_iteration` -> `meta.iteration`
- Line 911: `session.metadata.loop_iteration` -> `session.metadata.iteration`
- Line 912: `session.metadata.last_loop_next` -> `session.metadata.last_iteration_at`
- Line 913: `LoopLogEntry` -> `IterationLogEntry`
- Line 918: `session.metadata.loop_log` -> `session.metadata.iteration_log`
- Lines 920-922: `session.metadata.loop_log` -> `session.metadata.iteration_log` (cap logic)
- Line 941: `original_prompt.clone()` -> still `prompt` (already renamed via meta)
- Line 983: `inherit_loop_fields_from` -> `inherit_recurrence_from`

- [ ] **Step 2: Update session_agent.rs**

- Line 7: import already updated in Task 1
- Line 204: `s.metadata.loop_log` -> `s.metadata.iteration_log`
- Line 425: `meta.original_prompt` -> `meta.prompt`
- Line 455: `inherit_loop_fields_from` -> `inherit_recurrence_from`
- Lines 528-531: test assertions — rename field references

- [ ] **Step 3: Update nostr_transport.rs**

- Line 1455: `original_prompt: prompt.map(String::from)` -> `prompt: prompt.map(String::from)`
- Lines 1819-1825: `original_prompt` -> `prompt`, `loop_iteration` -> `iteration`, `loop_log` -> `iteration_log`, `last_loop_next` -> `last_iteration_at`
- Line 1833: `original_prompt` -> `prompt`

- [ ] **Step 4: Update api.rs status JSON**

Lines 103-106: rename JSON keys:

```rust
"prompt": s.metadata.prompt,
"iteration": s.metadata.iteration,
"iteration_log": s.metadata.iteration_log,
"last_iteration_at": s.metadata.last_iteration_at,
```

- [ ] **Step 5: Update admin.rs dashboard**

Lines 70-71: `s.metadata.loop_iteration` -> `s.metadata.iteration`

- [ ] **Step 6: Run `cargo test` and `cargo clippy`**

Run: `cargo test 2>&1 | tail -10` and `cargo clippy 2>&1 | tail -10`
Expected: PASS — all field renames complete

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "update all consumers to use renamed iteration fields"
```

---

### Task 6: Add OnFire::kills_alive() and fix NewSession doc comment

**Files:**
- Modify: `src/scheduler.rs:19-63` (OnFire impl)

- [ ] **Step 1: Write test for kills_alive()**

Add in `src/scheduler.rs` tests:

```rust
#[test]
fn on_fire_kills_alive() {
    assert!(!OnFire::ContinueSession.kills_alive());
    assert!(!OnFire::NewSession.kills_alive());
    assert!(!OnFire::PersistentWorktree { clear_context: false }.kills_alive());
    assert!(OnFire::PersistentWorktree { clear_context: true }.kills_alive());
    assert!(OnFire::DisposableWorktree.kills_alive());
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — method doesn't exist

- [ ] **Step 3: Add kills_alive() to OnFire**

After `is_disposable_worktree()`:

```rust
    /// Whether this mode kills an alive session's process on each fire.
    /// Only worktree modes with context clearing need to kill alive sessions.
    /// ContinueSession and NewSession are no-ops when alive (reminder handles nudging).
    pub fn kills_alive(&self) -> bool {
        match self {
            Self::ContinueSession | Self::NewSession => false,
            Self::PersistentWorktree { clear_context } => *clear_context,
            Self::DisposableWorktree => true,
        }
    }
```

- [ ] **Step 4: Update NewSession doc comment** (line 27)

Change from:
```rust
    /// Kill pane, start fresh conversation (no --continue/--resume).
    NewSession,
```
To:
```rust
    /// Start fresh when dead/missing; no-op when alive (reminder handles nudging).
    NewSession,
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test on_fire_kills_alive 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "add OnFire::kills_alive(), fix NewSession doc comment"
```

---

### Task 7: ScheduledTask — make message optional, add prompt/reminder

**Files:**
- Modify: `src/scheduler.rs:65-94` (ScheduledTask struct)
- Modify: `src/scheduler.rs:109-181` (custom Deserialize impl)
- Modify: `src/scheduler.rs:252-255` (format_scheduled_message)
- Modify: `src/scheduler.rs:302-314` (execute_task)
- Modify: `src/scheduler.rs:765-793` (new_task)
- Modify: `src/mcp.rs:128-151` (TaskCreateParams)
- Modify: `src/mcp.rs:612-641` (task_create handler)
- Modify: `src/api.rs:1006-1048` (CreateTaskBody + create_task)

- [ ] **Step 1: Write test for task with prompt/reminder and no message**

```rust
#[test]
fn new_task_with_prompt_no_message() {
    let task = new_task(
        "test-task".into(),
        "0 0 * * *".into(),
        None,
        None,
        Some("do the work".into()),
        Some("call loop_next".into()),
        false,
        None,
        OnFire::NewSession,
    );
    assert_eq!(task.prompt.as_deref(), Some("do the work"));
    assert_eq!(task.reminder.as_deref(), Some("call loop_next"));
    assert!(task.message.is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — new_task doesn't accept prompt/reminder, message is still required

- [ ] **Step 3: Update ScheduledTask struct**

Make `message` optional, add `prompt` and `reminder`:

```rust
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub cron: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session: Option<String>,
    /// Legacy: per-fire injection message. Omitted for new prompt-driven tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Bootstrap: prompt for creating/reviving the target session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Bootstrap: reminder for the target session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder: Option<String>,
    pub enabled: bool,
    // ... rest unchanged
```

- [ ] **Step 4: Update custom Deserialize impl Raw struct** (lines 109-181)

Change `message: String` to `#[serde(default)] message: Option<String>` in Raw. Add `#[serde(default)] prompt: Option<String>` and `#[serde(default)] reminder: Option<String>`. Thread through to ScheduledTask construction.

- [ ] **Step 5: Update new_task() signature** (lines 765-793)

Replace `message: String` with `message: Option<String>`, add `prompt: Option<String>`, `reminder: Option<String>`.

- [ ] **Step 6: Update execute_task()** (lines 302-314)

Change `format_scheduled_message(&task.message)` to handle `Option`:

```rust
let formatted = task.message.as_deref().map(format_scheduled_message);
let run = execute_injection(state, &task, formatted.as_deref()).await;
```

Update `execute_injection` signature to take `Option<&str>` instead of `&str`.

- [ ] **Step 7: Update TaskCreateParams in mcp.rs** (lines 128-151)

Remove `message: String`. Add:

```rust
    /// Bootstrap: prompt for creating/reviving the target session.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Bootstrap: reminder for the target session.
    #[serde(default)]
    pub reminder: Option<String>,
```

Update `task_create` handler to pass new params to `new_task()`.

- [ ] **Step 8: Update CreateTaskBody in api.rs** (lines 1006-1048)

Same changes: remove `message: String`, add optional `prompt` and `reminder`. Update `create_task()` handler.

- [ ] **Step 9: Run test to verify it passes**

Run: `cargo test new_task_with_prompt_no_message 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 10: Run `cargo test` and `cargo clippy`**

Expected: Some existing tests may need updating for new function signature.

- [ ] **Step 11: Commit**

```bash
git add -A && git commit -m "make task message optional, add prompt/reminder bootstrap fields"
```

---

### Task 8: Fix execute_injection — alive no-op, bootstrap on revival, prompt injection

**Files:**
- Modify: `src/scheduler.rs:334-397` (execute_injection)
- Modify: `src/scheduler.rs:413-491` (respawn_and_inject)
- Modify: `src/scheduler.rs:496-705` (revive_and_inject, revive_from_task)

- [ ] **Step 1: Fix alive-session path in execute_injection** (line 377)

Replace `task.on_fire.clears_context()` with `task.on_fire.kills_alive()`. Add backward compat shim for old message-only tasks:

```rust
if alive {
    if task.on_fire.kills_alive() {
        let dir = task.project_dir.as_deref()
            .or(session.metadata.project_dir.as_deref())
            .unwrap_or("/tmp");
        return respawn_and_inject(state, task, pane, dir, formatted).await;
    } else if let Some(ref fmt) = formatted {
        // Backward compat: old tasks with message but no prompt
        if task.prompt.is_none() {
            return inject_into_pane(state, task, pane, session.metadata.vim_mode, fmt).await;
        }
    }
    // New-style tasks: no-op when alive. Reminder handles nudging.
    return TaskRun::ok(task, None);
}
```

- [ ] **Step 2: Expand session-not-found guard** (line 346)

Change `if task.project_dir.is_some()` to `if task.project_dir.is_some() || task.prompt.is_some()`.

- [ ] **Step 3: Fix revive_and_inject() to stamp bootstrap fields** (line 678)

Replace the minimal `proto_meta` with:

```rust
let proto_meta = crate::daemon_protocol::SessionMeta {
    project_dir: project_dir.map(String::from),
    prompt: task.prompt.clone(),
    reminder: task.reminder.clone(),
    on_fire: Some(task.on_fire.clone()),
    ..Default::default()
};
```

- [ ] **Step 4: Add prompt injection on revival** (after line 702)

After the `locked_inject` for message, add prompt injection for new-style tasks:

```rust
// Inject prompt for new-style tasks (replaces message injection)
if let Some(ref prompt) = task.prompt {
    let full_text = match &task.reminder {
        Some(r) => format!("{prompt}\n\n{r}"),
        None => prompt.clone(),
    };
    crate::nostr_transport::schedule_prompt_injection(state, task.session_name(), new_pane.clone(), full_text);
} else if let Some(ref fmt) = formatted {
    // Legacy: inject formatted message for old tasks
    tmux::locked_inject(state, task.session_name(), &new_pane, fmt, false).await?;
}
```

Note: `schedule_prompt_injection` needs to be made `pub` in `nostr_transport.rs` if not already.

- [ ] **Step 5: Fix respawn_and_inject() to stamp bootstrap** (after line 466)

Replace the `backend_session_id` clear block with the expanded version from the spec.

- [ ] **Step 6: Run `cargo test` and `cargo clippy`**

Run: `cargo test 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "fix scheduler: alive no-op, bootstrap on revival, prompt injection"
```

---

### Task 9: Update e2e tests for renamed JSON fields

**Files:**
- Modify: `tests/e2e/run-tests.sh` (tests 27-31)
- Modify: `tests/e2e/run-opencode-tests.sh` (test 13)

- [ ] **Step 1: Update run-tests.sh field references**

Replace in jq queries:
- `.original_prompt` -> `.prompt`
- `.loop_iteration` -> `.iteration`
- `.loop_log` -> `.iteration_log`
- `.last_loop_next` -> `.last_iteration_at`

Update test descriptions and comments accordingly.

- [ ] **Step 2: Update run-opencode-tests.sh field references**

Same replacements.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "update e2e tests for renamed iteration fields"
```

---

### Task 10: Update MCP instructions (OUIJA_INSTRUCTIONS)

**Files:**
- Modify: `src/mcp.rs:1094+` (OUIJA_INSTRUCTIONS const)

- [ ] **Step 1: Update `<tasks>` section**

Replace the tasks documentation to reflect prompt/reminder bootstrap fields, remove references to `message` as the primary mechanism, document that cron tasks ensure liveness and prompt drives the work.

- [ ] **Step 2: Update `<loops>` section**

Update field names in the documentation: `original_prompt` -> `prompt`, describe the unified model.

- [ ] **Step 3: Run `cargo test` and `cargo clippy`**

Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "update MCP instructions for unified recurring sessions"
```

---

### Task 11: Update README with unified recurring sessions understanding

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add/update section on long-running sessions**

Document the unified model:
- The prompt is the control plane — it defines the workflow and clean_context strategy
- Two orthogonal triggers: active (loop_next) and passive (cron), same underlying machinery
- The daemon compensates for LLM fragility via reminder injection, stall detection, force-restart
- Tasks bootstrap sessions with prompt + reminder; the session owns runtime state
- Example: autoresearch pattern with external INSTRUCTIONS.md + results.tsv

- [ ] **Step 2: Commit**

```bash
git add README.md && git commit -m "README: document unified recurring sessions model"
```

---

### Task 12: Run full e2e test suite

**Files:**
- Run: `tests/e2e/run-tests.sh`

- [ ] **Step 1: Run the local e2e tests**

Run: `tests/e2e/run-tests.sh 2>&1 | tail -20`
Expected: All tests pass, especially tests 27-31 (loop/iteration), L11-L13 (on_fire), test 31 (re-registration)

- [ ] **Step 2: Fix any failures**

If tests fail, fix the root cause and recommit.

- [ ] **Step 3: Final `cargo clippy`**

Run: `cargo clippy 2>&1 | tail -10`
Expected: No warnings
