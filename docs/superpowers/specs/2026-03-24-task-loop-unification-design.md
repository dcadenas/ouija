# Unified Recurring Sessions

Unify ouija's scheduled tasks and loop_next into a single recurring session primitive.

## Problem

Ouija has two systems that do the same thing with different names:

- **loop_next** (self-triggered): session calls it to advance, optionally restarting with clean context. Tracks iterations, has stall detection, uses prompt + reminder.
- **Scheduled tasks** (cron-triggered): external trigger on a cron schedule, injects a message into a session. Has run_count but no prompt, no reminder, no stall detection.

These are isomorphic. Both do: "advance a recurring session -- if context is fine, continue. If stale or dead, restart fresh with the original prompt." The only difference is the trigger: self vs cron.

## Insight

The prompt is the control plane. It defines the workflow, the iteration strategy, the clean_context policy. The daemon provides dumb primitives -- the prompt orchestrates them.

Two orthogonal modes of long-running sessions:

- **Active** (loop_next): no idle. Session finishes work, calls loop_next, continues. The session drives its own pace. Runs forever as long as it keeps calling loop_next.
- **Passive** (cron): idle between fires. Cron triggers, session wakes, does work, goes idle. Cron triggers again. Runs forever on a schedule.

Both use the same machinery: prompt + reminder + on_fire + iteration tracking + stall detection. The divine-perf autoresearch experiment demonstrates the active pattern: prompt points to INSTRUCTIONS.md, session reads external state, does one iteration, writes results, calls loop_next. No per-fire "message" needed -- the prompt drives everything.

The `message` field on ScheduledTask was a workaround for tasks not having prompts. With prompts on tasks, `message` is redundant:
- For `new_session` / clean context: the prompt drives the work.
- For `continue_session` / live nudging: that's what `reminder` does (idle re-injection, every-10th-iteration inclusion, stall nudges).
- For ad-hoc communication: `session_send` already exists.

## Unified Model

```
Recurring Session = prompt + reminder + on_fire
  trigger: Self (loop_next) | Cron(expression)
  state: iteration, iteration_log, last_iteration_at
  safety: stall detection (3x avg -> mild nudge, 10x/30min -> force restart)
```

## Design: Session Owns Runtime, Task Bootstraps

The session owns all runtime recurrence state. A scheduled task is a cron trigger with bootstrap configuration for creating/reviving sessions.

### Data Model Changes

Two parallel metadata types change in lockstep:
- `SessionMeta` in `daemon_protocol.rs` (wire/persistence, i64 timestamps, Hash+Eq)
- `SessionMetadata` in `state.rs` (runtime, DateTime<Utc>)
- `metadata_to_session_meta()` maps between them

**Renames on both types** (serde aliases for backward compat):

| Old | New | Alias |
|---|---|---|
| `original_prompt` | `prompt` | `alias = "original_prompt"` |
| `loop_iteration` | `iteration` | `alias = "loop_iteration"` |
| `loop_log` | `iteration_log` | `alias = "loop_log"` |
| `last_loop_next` | `last_iteration_at` | `alias = "last_loop_next"` |

**New field on session metadata:**

```rust
/// How this session handles recurrence. None = not a recurring session.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub on_fire: Option<OnFire>,
```

**Rename:** `LoopLogEntry` -> `IterationLogEntry` (serde alias on containing fields).

**Rename:** `inherit_loop_fields_from()` -> `inherit_recurrence_from()` (same logic, new names).

### ScheduledTask Changes

**Remove `message`** (required -> absent). Existing persisted tasks with `message` still deserialize via `#[serde(default)]` but the field is ignored for new task creation.

**Add bootstrap fields:**

```rust
/// Bootstrap: prompt for creating/reviving the target session.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub prompt: Option<String>,
/// Bootstrap: reminder for the target session.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub reminder: Option<String>,
```

These are used only when creating or reviving a dead session. The session then owns these values at runtime. If the task's bootstrap values are updated later, they take effect on the next revival.

**`new_task()` signature:** Remove `message`, add `prompt` and `reminder`.

### NewSession On-Fire Behavior Fix

Current bug: `execute_injection()` checks `task.on_fire.clears_context()` for alive sessions. `NewSession` returns true, so alive sessions get killed + respawned on every cron fire. This destroys a working session.

**Fix:** Add `OnFire::kills_alive()`:

```rust
pub fn kills_alive(&self) -> bool {
    match self {
        Self::ContinueSession | Self::NewSession => false,
        Self::PersistentWorktree { clear_context } => *clear_context,
        Self::DisposableWorktree => true,
    }
}
```

Replace `task.on_fire.clears_context()` with `task.on_fire.kills_alive()` in the alive-session branch of `execute_injection()`. `clears_context()` remains for the dead-session revival path.

**Resulting behavior:**

| OnFire | Alive | Dead/Missing |
|---|---|---|
| ContinueSession | do nothing (reminder handles nudging) | revive with --continue/--resume, apply prompt+reminder |
| NewSession | do nothing (reminder handles nudging) | start fresh, apply prompt+reminder |
| PersistentWorktree(clear=false) | do nothing | resume in worktree, apply prompt+reminder |
| PersistentWorktree(clear=true) | respawn in worktree | start fresh in worktree |
| DisposableWorktree | respawn in disposable worktree | start fresh in disposable worktree |

Note: for ContinueSession and NewSession, "do nothing" on alive sessions means the reminder system handles any nudging. The cron fire just ensures liveness.

### Bootstrap Flow: Task Revives Session With Recurrence State

`revive_and_inject()` currently re-registers with minimal metadata:

```rust
let proto_meta = SessionMeta {
    project_dir: project_dir.map(String::from),
    ..Default::default()  // prompt, reminder, on_fire all None
};
```

**Fix:** Apply task's bootstrap fields:

```rust
let proto_meta = SessionMeta {
    project_dir: project_dir.map(String::from),
    prompt: task.prompt.clone(),
    reminder: task.reminder.clone(),
    on_fire: Some(task.on_fire.clone()),
    ..Default::default()
};
```

`inherit_recurrence_from()` in `apply_register` merges these with any existing state.

Same fix in `respawn_and_inject()`: stamp prompt/reminder/on_fire on session metadata after respawn.

### Task Execution Flow (Revised)

Without `message`, the scheduler's job simplifies to: ensure session liveness.

```
execute_injection(state, task):
  session = lookup(task.session_name())

  if session not found:
    if task has project_dir or prompt:
      revive_from_task(state, task)    # creates session with prompt+reminder
    else:
      fail("no info to create session")
    return

  if session alive:
    if task.on_fire.kills_alive():
      respawn_and_inject(state, task)  # worktree modes that need fresh process
    else:
      no-op                            # reminder system handles nudging
    return

  # session dead:
  if task.on_fire.clears_context():
    revive fresh with prompt+reminder
  else:
    revive with --continue/--resume, apply prompt+reminder
```

The `format_scheduled_message()` wrapper and `[scheduled task]: {message}` injection are removed.

### Prompt Injection on Revival

When a task creates/revives a session with a prompt, the prompt must be injected into the pane (same as `session_start` does). The revival flow in `revive_and_inject()` needs to:

1. Launch backend in new tmux pane
2. Wait for backend readiness
3. Re-register with prompt+reminder+on_fire in metadata
4. If prompt is present, inject `prompt + "\n\n" + reminder` (same concatenation as `start_session`)

This replaces the current `message` injection step.

### API Changes

**`TaskCreateParams` (MCP):**
- Remove `message` (was required)
- Add `prompt: Option<String>`
- Add `reminder: Option<String>`

**`CreateTaskBody` (HTTP API):** Same changes.

**`LoopNextParams`:** Unchanged. Reads from session's renamed metadata fields.

**`SessionNameParams` (session_start):** Unchanged. Already has `prompt` and `reminder`.

**`/api/status` response:** Rename JSON fields: `original_prompt` -> `prompt`, `loop_iteration` -> `iteration`, `loop_log` -> `iteration_log`, `last_loop_next` -> `last_iteration_at`.

**MCP instructions** (`OUIJA_INSTRUCTIONS`): Update `<tasks>` and `<loops>` sections to reflect unified model.

### Stall Detection

No changes needed. Stall detection watches `loop_next` calls on session metadata. Works identically regardless of trigger source.

Pure cron tasks (no loop_next) don't need stall detection -- the cron schedule IS the heartbeat. Hybrid sessions (cron + loop_next) get stall detection naturally when the session calls loop_next.

### What Stays the Same

- Stall detection logic in `session_agent.rs` (works on session metadata)
- `SessionMsg` enum variants (LoopProgress, LoopMildStall, LoopHardStall)
- `compute_average_loop_interval()` (renamed log field, same logic)
- `restart_session()` in `nostr_transport.rs` (carries forward loop state via prev_metadata)
- Persistence format (serde aliases handle old field names)

### Files Changed

| File | Changes |
|---|---|
| `daemon_protocol.rs` | Rename SessionMeta fields + aliases, LoopLogEntry -> IterationLogEntry, inherit_loop_fields_from -> inherit_recurrence_from, add on_fire |
| `state.rs` | Mirror renames on SessionMetadata, add on_fire |
| `scheduler.rs` | Add OnFire::kills_alive(), add prompt/reminder to ScheduledTask, remove message, fix execute_injection() alive path, fix revive_and_inject()/respawn_and_inject() to stamp bootstrap fields, inject prompt on revival, remove format_scheduled_message() |
| `mcp.rs` | Remove message from TaskCreateParams, add prompt/reminder, pass through to new_task(), update loop_next to renamed fields, update OUIJA_INSTRUCTIONS |
| `api.rs` | Remove message from CreateTaskBody, add prompt/reminder, rename fields in /api/status JSON |
| `session_agent.rs` | Update field references to new names |
| `nostr_transport.rs` | Update field references in start_session() and restart_session() |

### Migration

- **Serde aliases** handle persisted session state: old field names deserialize into new fields.
- **ScheduledTask.message** becomes `Option<String>` with `#[serde(default)]` for deserialization of existing persisted tasks. Old tasks with `message` but no `prompt` continue to work: the message is injected as before (backward compat shim in execute_injection). New tasks created via API don't have message.
- **NewSession behavior change**: sessions that relied on NewSession to force-kill alive sessions now get no-op instead. This is intentional (the old behavior was a bug).

### Testing

1. **Serde alias migration**: deserialize old-format JSON with original_prompt/loop_iteration/loop_log/last_loop_next
2. **OnFire::kills_alive()**: unit test (ContinueSession=false, NewSession=false, PersistentWorktree(true)=true, DisposableWorktree=true)
3. **Task with prompt/reminder**: create via API, trigger, verify session metadata gets prompt+reminder+on_fire
4. **NewSession alive fix**: register alive session + NewSession task, trigger, verify session NOT killed
5. **Prompt injection on revival**: task with prompt fires on dead session, verify prompt injected into pane
6. **Backward compat**: old task with message and no prompt still injects message on fire
7. **Existing e2e tests pass**: tests 27-31 (loop_next), L11-L13 (on_fire), test 31 (re-registration preserves state)
8. **Hybrid mode**: task creates session with prompt, session calls loop_next, stall detection works
