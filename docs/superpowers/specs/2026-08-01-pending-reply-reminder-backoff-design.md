# Pending-Reply Reminder Backoff Design

## Purpose

Ouija must not turn a fast backend failure into an unbounded pending-reply
reminder loop. An unanswered question may continue to wake a worker, but Ouija
must attempt to remind that worker about a specific pending message at most
once per configured idle timeout.

This change is limited to reminder scheduling in the per-session actor. It does
not change message delivery, reply completion, persistent pending-reply
semantics, manual task reminders, lifecycle policy, or backend error handling.

## Confirmed Failure

`SessionMsg::Stopped` currently does two things when a session owes a reply:

1. it arms `IdleTimeout` for the configured `idle_timeout_secs`;
2. it immediately calls `send_reminders` for every pending entry whose
   `last_activity` is already older than that timeout.

Reminder delivery does not record a cooldown. If an OpenCode turn fails before
provider inference, OpenCode quickly emits an error and returns the session to
idle. Ouija observes another stopped boundary roughly 200 milliseconds later.
The same pending entry remains overdue, so the immediate path injects it again.
This repeats without waiting for the idle timer.

The production incident alternated identical pending-reply reminders with
zero-part, zero-token assistant records every 180–250 milliseconds. The
OpenCode records were a pre-existing transcript behavior after a project-tool
resolution failure; they were not introduced by the stream-liveness changes.
Ouija must remain safe for any backend that fails quickly, regardless of how
that backend records the failed turn.

## Design

### Actor-local per-message cooldown

`SessionAgentState` will track the most recent reminder attempt for each
pending message identity:

```text
(sender public ID, message ID) -> monotonic attempt time
```

The actor uses monotonic time so wall-clock adjustments cannot shorten the
cooldown. The configured `idle_timeout_secs` is both the ordinary idle delay
and the minimum interval between attempts for the same pending message.

Both pending-reply delivery sites use one shared throttled dispatcher:

- the immediate overdue check after `SessionMsg::Stopped`;
- the pending-reply portion of `SessionMsg::IdleTimeout`.

For each still-pending entry, the dispatcher:

1. rejects stale actor ownership as today;
2. skips the entry when its previous attempt is newer than one idle timeout;
3. records the new attempt before calling the delivery primitive;
4. attempts the existing formatted reminder delivery.

Recording before delivery is deliberate. A failed injection or a backend that
accepts delivery and immediately fails must still back off. The next retry may
occur after one idle timeout.

Cooldown is per message rather than per session. A newly received question can
be reminded independently even when another unanswered question is cooling
down.

### Cleanup and lifecycle

After reading the current pending entries, the actor removes cooldown keys that
no longer correspond to a pending `(sender, message ID)`. A completed reply,
explicit pending-reply clear, or sender removal therefore releases its
actor-local state without changing daemon protocol state.

Cooldown state is intentionally not persisted. Replacing or restarting the
session actor may produce one immediate reminder for an already-overdue entry,
but the new actor then enforces the full timeout. This bounded duplicate is
preferable to adding persistence schema, migration, and pure-state-machine
events for transient delivery scheduling.

Renames preserve the actor when the exact owner incarnation and pane remain
valid. Because pending-reply sender IDs and target ownership already migrate
through the daemon protocol, the actor prunes any obsolete cooldown identity
on its next stopped or idle boundary.

### Unchanged behavior

- `ouija ask` continues to create a pending reply and may wake a worker.
- `ouija tell` continues to create no pending reply.
- Omitting `--reminder` disables only durable task reminders; pending replies
  remain an independent wakeup source.
- `--when-done ask-parent` continues to provide completion instructions without
  creating a pending reply by itself.
- Progress replies continue to update `PendingReplyEntry.last_activity`.
- Completed replies continue to remove their pending entry.
- Manual reminders continue to use their existing clearing ID and idle-timer
  behavior.
- The pure `DaemonState::apply` transition model and persisted state format do
  not change.

## Alternatives Rejected

### Remove the immediate overdue path

Relying only on `IdleTimeout` is smaller, but it delays an already-overdue
question for a full additional timeout whenever a legitimate long-running turn
stops. The immediate path is useful when it is rate-limited correctly.

### Persist `last_reminded_at`

Adding a field to `PendingReplyEntry` would retain cooldown across daemon
restarts, but reminder delivery is not conversation activity. Reusing
`last_activity` would corrupt its meaning, while a new persistent field would
require state-machine events, migrations, model updates, and durable writes for
a transient scheduling concern.

### Backend-specific empty-message handling

Special-casing OpenCode zero-token messages would leave the same loop possible
for other fast failures. OpenCode should separately finalize assistant records
when ordinary pre-inference failures occur, but Ouija's reminder cadence must
not depend on backend transcript representation.

## Verification

Implementation follows test-driven development.

Focused actor tests will use paused Tokio time and the existing OpenCode
reminder test transport to prove:

1. repeated stopped boundaries for one overdue pending reply inject exactly one
   reminder within the idle timeout;
2. a second reminder becomes eligible only after the full idle timeout;
3. two different pending message identities are throttled independently;
4. clearing a pending reply prunes its cooldown state;
5. manual reminders retain their existing clearing-ID behavior;
6. lifecycle-only metadata still does not arm recurring reminders.

The red regression must reproduce multiple rapid `Stopped` events and observe
duplicate deliveries on the current implementation before production code is
edited. After the minimal fix, the focused session-agent tests, the complete
session-agent module, `cargo check --tests`, formatting, Clippy, and the normal
non-ignored test suite must pass. The CPU-intensive ignored Stateright model is
not required because this design does not change `DaemonState`, protocol
events, effects, persistence, or model state.

## Operational Guidance Before Release

Until the fix is deployed, coordinators starting failure-prone workers should
omit `--reminder`, supply the complete assignment through the stored
`--prompt`, and use `ouija tell` rather than `ouija ask` for follow-ups that do
not require a tracked answer. This avoids creating the pending-reply condition;
it is an operational mitigation, not the permanent fix.
