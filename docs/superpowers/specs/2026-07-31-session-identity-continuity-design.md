# Session Identity Continuity Design

## Purpose

Ouija must preserve a Local session's public identity when a backend thread
survives the death and reaping of its tmux pane. The same complete backend
identity reopening in a replacement pane must recover the prior arbitrary
public ID and persistent lifecycle metadata with a new daemon-issued
incarnation.

A genuinely new Local assistant that has never been registered must also be
able to choose a free public ID through an explicit self-claim operation:

```text
ouija claim <requested-id>
```

Ordinary rename remains strict. Its source must be an existing Local session
owned by the verified caller. A missing rename source never becomes authority
to create or claim an ID.

This design changes only the Local control plane. Nostr ingress continues to
derive identity solely from authenticated transport provenance and cannot
recover a tombstone, self-claim, or set Local caller evidence.

## Confirmed Failure

`Event::ReapDead` currently removes the only `SessionEntry` containing the
public ID, complete backend pair, project, and lifecycle metadata. The
persisted snapshot then contains neither that row nor any historical identity
record. `incarnation_high_water` prevents token reuse but does not retain the
deleted identity association.

The existing missing-pane reclaim path works only while the canonical live row
still exists. The existing `recover-backend-identity` operator path works only
for an existing Local row whose backend fields are both blank. Neither path can
recover a reaped complete row.

For home-managed worktrees such as
`$HOME/.ouija/worktrees/ouija/rootfix`, generic SessionStart additionally
reduces the cwd to `$HOME` and rejects it through the home-cwd guard. Fixing
that string heuristic alone would still derive identity from a basename and
would not restore an arbitrary public ID or its metadata.

Ordinary `apply_rename` also removes the source and inserts it at the
destination without first rejecting an occupied destination. That overwrites
the destination owner and deletes the source key.

## State Model

### Durable tombstone

Add a separate durable map to `DaemonState`, keyed by public Local session ID:

```rust
pub dormant_sessions: BTreeMap<String, DormantSession>
```

`DormantSession` contains:

- the public session ID;
- the prior exact `ResourceOwner`;
- the complete backend name and backend session ID through preserved metadata;
- the original project directory in metadata and a separately captured
  canonical project identity used to corroborate a replacement pane;
- all persistent lifecycle metadata already carried by `SessionMeta`, including
  role, bulletin, prompt, reminder, parent, completion policy, model, effort,
  worktree, recurrence, and active-context fields.

It contains no live pane, session agent, pending-reply bucket, runtime stopped
boundary, managed-launch credential, backend-repair reservation, or authority
to deliver messages or mutate external resources.

Only an exact reaper transition may create a tombstone. The reaped row must:

- be the exact current Local `ResourceOwner` and expected pane;
- have no conflicting lifecycle lease;
- contain one complete backend pair;
- contain a usable canonical project directory.

Rows without a complete backend pair or canonical project cannot be recovered
safely and continue to be removed without a tombstone.

Tombstones are not included in discovery, status session lists, delivery
routing, session caps, activity accounting, Nostr announcements, or agent
ownership. They reserve their public ID and backend pair solely for exact
recovery.

Every generic registration, managed start reservation, backend bind, rename,
and Local claim treats a tombstoned public ID or backend pair as occupied.
Only exact tombstone recovery or explicit dormant unregister may consume the
reservation.

### Retention and cleanup

Tombstones have no TTL. A time-based expiry would recreate the original
identity-loss failure after a sufficiently long pause.

An exact successful recovery atomically consumes its tombstone. An explicit
operator unregister of a dormant public ID removes that tombstone and releases
its ID/backend reservation. Removing a dormant tombstone does not delete its
preserved worktree; reaping must remain non-destructive, and a historical
owner token is not sufficient worktree-cleanup authority.

Explicit removal of a live session does not create a tombstone. Only
`ReapDead` represents the unplanned pane-loss case that needs continuity.

## Exact Tombstone Recovery

Complete backend identity is the primary recovery key. Live identity
resolution remains unchanged for `whoami` and message sending; dormant lookup
is a separate internal operation so a tombstone never appears live.

SessionStart performs these steps in order:

1. Validate the adapter's complete backend identity and confirm that the
   reported pane is a live assistant running that backend.
2. Resolve the identity against live Local rows. The existing missing-pane
   reclaim/current-owner behavior remains first for live rows.
3. If no live row owns the pair, resolve it against tombstones.
4. If a tombstone matches, attempt exact recovery before basename derivation
   and before the home-cwd guard.
5. If no tombstone matches, continue ordinary new-session discovery.

Recovery holds resource gates for the replacement pane, backend session, and
canonical project while it performs physical inspection and the final pure
state transition. The transition compares the exact tombstone owner, public
ID, backend pair, and project. It also rechecks that:

- the prior public ID is not live and has no lifecycle lease;
- the replacement pane is not owned or reserved;
- the backend pair is not owned or reserved elsewhere;
- the canonical project is unchanged and not lease-conflicted;
- the tombstone itself is unchanged.

On success, the transition allocates an incarnation strictly greater than the
tombstone's prior incarnation, restores the persistent metadata, binds the new
pane, removes the tombstone, persists the complete state, then emits pane
markers, agent startup, and the normal Local session re-announcement/list
effects for the new owner.

Persistence is part of the commit boundary. If persistence fails, the protocol
state rolls back to the tombstone and no external success effects run.

An exact retry after recovery resolves the live backend pair and same pane and
returns the current owner without allocating another incarnation or emitting
replacement effects.

If an exact tombstone backend pair exists but physical or ownership
corroboration fails, SessionStart fails closed. It must not fall through to
generic registration or permit the same backend to claim a different public
ID.

## Explicit Local Self-Claim

### CLI and endpoint

The CLI interface is:

```text
ouija claim <requested-id>
```

The CLI discovers the current adapter's typed backend identity and the current
tmux pane evidence. The raw backend-native ID is sent only in the JSON body to
`POST /api/session-identities/claim` and is never placed in argv or treated as
a public ID.

The Local endpoint accepts:

```json
{
  "requested_id": "chosen-public-id",
  "caller": {
    "pane": "%819",
    "pane_var_id": null,
    "env_id": null,
    "backend_identity": {
      "backend": "codex-cli",
      "session_id": "opaque-backend-id"
    }
  }
}
```

The caller object is a dedicated `LocalClaimEvidence` contract rather than the
message-sending `SenderContext`. Keeping pane-variable and environment IDs
separate ensures one conflicting positive signal cannot be hidden by
resolution precedence.

The endpoint itself is the Local control-plane boundary. It is not registered
as a Nostr command and remote wire messages cannot construct or invoke the
claim transition.

The requested ID must be non-empty and contain no `/`. It is the sole name
intent; there is no source ID and no rename alias.

### Required evidence

Self-claim requires all of the following positive evidence:

- exactly one complete adapter identity;
- one current tmux pane;
- a fresh assistant-pane observation for that pane;
- a process matching the named backend adapter;
- a usable, canonical, non-root project path from the live pane;
- no positive pane, environment, marker, or backend observation resolving the
  caller to a different Local owner.

The daemon derives project metadata from the live pane. Caller-supplied project
text is not authoritative.

The coordinator holds pane, backend-session, and project resource gates across
inspection, compare-and-swap, durable persistence, and effect scheduling.

### Recovery precedence and retry

Before creating a new claim, the endpoint checks whether the complete backend
identity has a tombstone. If it does, exact tombstone recovery takes
precedence and restores the tombstone's prior public ID even when the requested
ID differs.

If the backend identity already resolves to a live Local session:

- the same requested ID, pane, backend pair, and canonical project is an
  idempotent retry and returns the current owner;
- any different requested ID is rejected as already registered and is not an
  implicit rename.

For a genuinely unregistered backend with no tombstone, the final pure claim
transition atomically verifies the requested destination, pane, backend pair,
project, and leases again, allocates one new incarnation, creates one Local
row, persists it, then publishes pane and agent effects.

## Conflict Tables

### Tombstone recovery

| Observed state | Result |
|---|---|
| Exact tombstone pair, unchanged project, free replacement pane and ID | Recover prior public ID with a newer incarnation |
| Same recovered live owner, pane, pair, and project retries | Return current owner; no mutation |
| Tombstone pair exists but prior public ID is live | Reject; do not overwrite or create another ID |
| Replacement pane is owned or reserved | Reject; do not evict |
| Backend pair is live, reserved, or duplicated | Reject |
| Project identity differs or is lease-conflicted | Reject |
| Tombstone changed or disappeared after inspection | Reject as superseded |
| Persistence fails | Roll back to unchanged tombstone; return failure |

### New Local self-claim

| Observed state | Result |
|---|---|
| Complete unregistered backend evidence and all resources are free | Create requested Local ID |
| Exact tombstone owns the backend pair | Recover tombstone public ID instead |
| Exact live requested owner retries from same pane/pair/project | Return current owner; no mutation |
| Requested ID is a live session of any origin | Reject |
| Requested ID is tombstoned by a different backend pair | Reject |
| Caller pane belongs to or is reserved by another owner | Reject |
| Backend pair belongs to or is reserved by another owner | Reject |
| Positive self-ID, environment, marker, or backend evidence identifies a sibling | Reject |
| Any lifecycle lease conflicts by ID, pane, backend pair, or canonical project | Reject |
| Backend process or canonical project cannot be corroborated | Reject |
| Persistence fails | Roll back; return failure |

### Ordinary rename

| Observed state | Result |
|---|---|
| Verified existing Local source and free live/tombstone destination | Rename source |
| Missing source | Reject; never claim |
| Remote or Human source | Reject |
| Live destination occupied by any origin | Reject without changing either row |
| Tombstoned destination occupied | Reject without changing source or tombstone |
| Source or destination has a lifecycle lease | Reject |
| Same source and destination | Idempotent success for the verified current owner |

## Pure State Transitions and Side Effects

The pure state machine gains explicit events for:

- consuming one exact tombstone into a recovered Local row;
- claiming one free public ID for a corroborated unregistered Local backend.

Neither event bumps `last_metadata_update`. Tombstone recovery preserves the
existing user-facing metadata timestamp; self-claim creates initial metadata
but does not pretend that the user updated role or bulletin.

Both events emit structured success acknowledgements so callers never infer
success from generic persistence effects. They must reject conflicts before
any map removal or insertion. They must not call ordinary `apply_register`,
whose pane-dedup behavior is intentionally unsuitable for fail-closed recovery
and claim.

Resource inspection and durable rollback remain in `AppState`. Tmux markers,
agent changes, persistence, and broadcasts remain effects executed only after
the protocol lock is released.

## Persistence Compatibility

The persisted lifecycle snapshot gains a serde-defaulted tombstone map. Saving
the new schema writes version 2.

Loading supports:

- legacy unversioned session arrays;
- version 1 snapshots without tombstones, migrated to an empty tombstone map;
- version 2 snapshots with tombstones.

Version 2 prevents an older binary from silently loading and later dropping
tombstones. A downgrade to a version-1-only binary fails on the unknown
snapshot version instead of erasing continuity state.

Load-time validation rejects:

- a tombstone whose map key, embedded ID, and prior owner ID disagree;
- incomplete backend pairs or missing/unsafe project identity;
- duplicate public IDs across live and dormant state;
- one backend pair assigned to distinct live, dormant, or lifecycle owners.

Normalization raises the high-water mark to cover live sessions, tombstones,
restart snapshots, and lease resource owners.

Startup restores tombstones before live-session reconciliation. Abandoned
lease cleanup treats a valid tombstone as a resource sharer and must not delete
its backend session or project directory. Reconciled persistence writes the
tombstone map unchanged.

## Test and Model-Check Plan

The already-red regressions remain the first proof:

- SessionStart after `%802` is reaped and the exact Codex thread appears in
  `%819` restores `rootfix`, its lifecycle metadata, and a newer incarnation;
- an occupied ordinary rename preserves both source and destination;
- an explicit verified claim creates a requested free ID;
- exact claim retry is idempotent;
- a claimant cannot take an occupied requested ID;
- focused Stateright exploration finds the current occupied-rename overwrite.

Additional pure-state tests cover:

- reaping a complete eligible row creates one tombstone and no live row;
- rows with incomplete identity or unusable project metadata do not create a
  recoverable tombstone;
- exact recovery consumes one tombstone and preserves every persistent
  lifecycle field;
- recovered incarnation is strictly newer;
- recovery retry does not allocate again;
- explicit dormant unregister releases the reservation without worktree
  cleanup;
- live/tombstone destination, foreign pane, foreign backend pair, conflicting
  project/ID lease, stale tombstone owner, and persistence rollback all fail
  without mutation.

API and CLI tests cover:

- `ouija claim <requested-id>` parsing and request construction;
- missing or incomplete adapter identity;
- missing/non-assistant pane, backend-process mismatch, project mismatch, and
  conflicting positive caller evidence;
- recovery precedence when the requested ID differs from the tombstone ID;
- exact idempotent retry;
- Local-only routing with no Nostr command path;
- response status and public ID for claimed, recovered, current, conflict, and
  persistence-failure outcomes.

Persistence tests cover version-1 migration, version-2 round-trip, high-water
normalization, validation of malformed/colliding tombstones, preservation
during abandoned-lease reconciliation, and rollback on write failure.

Stateright properties cover:

- no public destination is overwritten by rename, claim, or recovery;
- each complete backend pair has at most one distinct Local owner across live
  sessions, tombstones, and lifecycle claims;
- recovery consumes a tombstone at most once;
- every recovered incarnation is greater than the tombstone incarnation;
- stale reap/recovery results cannot remove or replace a newer winner;
- reaping and dormant cleanup never emit worktree deletion.

Focused tests run during each red-green cycle. Final verification runs:

```text
cargo test
cargo test model_check_bfs -- --ignored --nocapture
cargo clippy --all-targets --all-features
```

E2E tests are not required for the initial implementation because they would
mutate shared daemon/tmux state and the behavior is covered at the real API,
state, persistence, and model boundaries. A later isolated Docker scenario may
exercise process-level restart continuity.

## Out of Scope

- Inferring public identity from project basename, worktree slug, or backend
  native ID alone;
- changing Nostr sender authentication or remote-session naming;
- converting arbitrary missing-source rename requests into claims;
- TTL, automatic garbage collection, or heuristic reassignment of tombstones;
- reviving incomplete legacy backend rows;
- changing managed-launch credential rules or the existing operator
  `recover-backend-identity` path;
- deleting preserved worktrees during reap or dormant reservation cleanup.
