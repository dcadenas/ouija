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
public ID, complete backend pair, project, and lifecycle metadata. Trusted
Claude `SessionEnd` has the same identity-loss result:
`hooks::session_end_inner` resolves the exact incarnation and then applies
`Event::RemoveOwned`. In both cases the persisted snapshot contains neither
the live row nor any historical identity record. `incarnation_high_water`
prevents token reuse but does not retain the deleted identity association.

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
- the daemon-observed dormancy timestamp and whether the source was reaping or
  trusted SessionEnd;
- a sanitized copy of all persistent lifecycle metadata already carried by
  `SessionMeta`, including role, bulletin, prompt, reminder, parent, completion
  policy, model, effort, worktree, recurrence, and active-context fields.

It contains no live pane, session agent, pending-reply bucket, runtime stopped
boundary, managed-launch credential, backend-repair reservation, or authority
to deliver messages or mutate external resources.

One exact-owner dormancy transition creates tombstones. Both the reaper and a
trusted backend `SessionEnd` call it; neither path first deletes the row through
`RemoveOwned`. The row must:

- be the exact current Local `ResourceOwner` and expected pane;
- have no conflicting lifecycle lease;
- contain one complete backend pair;
- contain a usable canonical project identity;
- carry the daemon observation timestamp at which dormancy was confirmed.

The transition's `expected_pane` is optional only for a genuinely paneless live
row and must equal the row's pane value exactly. `source` is the closed enum
`Reaped | TrustedSessionEnd`; it affects audit output, not recovery authority.

Rows without a complete backend pair or canonical project cannot be recovered
safely and continue to be removed without a tombstone. A stale reaper result or
stale SessionEnd for a superseded owner is a no-op.

For reaping, the observation timestamp is captured by the daemon sweep when it
confirms the pane dead. For trusted SessionEnd, the hook handler captures daemon
time after resolving the exact incarnation plus pane or backend session. The
hook payload does not choose the accounting timestamp.

The dormancy transition closes active-context accounting before preserving
metadata. It uses the same arithmetic as `ActiveContextStopped`: take
`active_context_segment_started_at`, compute a non-negative elapsed interval in
`i128`, convert to `u64` with backward time contributing zero, saturating-add it
to `active_context_accumulated_secs`, clear the open segment, and recompute
`active_context_restart_due` with the existing monotonic stopped semantics:
an already-due value stays due, and a configured positive limit becomes due
when the accumulated value reaches it. The dormant record is therefore parked;
dormant wall time never counts as active and already-observed active work is not
lost.

`active_context_accounting_provisional` is preserved during dormancy. Dormancy
is rejected while the corresponding lifecycle lease still exists, so the
normal restart rollback/finalization path remains authoritative. If a
post-launch owner is provisional after its lease has completed, dormancy closes
and parks its current counters but does not invent restart success. Exact
tombstone recovery is a successful SessionStart for the same surviving backend
pair, so recovery clears that carried provisional marker only after the durable
recovery commit. It preserves the parked accumulated/due values and leaves
`active_context_segment_started_at = None`; it emits no due notice itself.
Only a later `SessionMsg::Active` for the recovered owner opens a new segment,
and the next exact stopped boundary may deliver an already-due notice.

Tombstones are not included in routable discovery, normal status session lists,
delivery routing, session caps, live activity accounting, Nostr announcements,
or agent ownership. They reserve their public ID and backend pair solely for
exact recovery.

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

No-TTL reservations are separately observable through the Local operator
surface:

```text
ouija dormant list
ouija dormant show <exact-id>
```

These commands use Local-only `GET /api/session-identities/dormant` and
`GET /api/session-identities/dormant/<encoded-id>`. They return the public ID,
prior incarnation, backend name, an opaque/redacted backend-session
fingerprint, original project directory, canonical project identity, dormancy
timestamp/source, and parked active-context state. They do not return launch
credentials, repair reservations, pending messages, or make the record
routable.

The intentional forget command remains the existing exact operator operation:

```text
ouija unregister <exact-id>
```

For a dormant target it returns the distinct success result
`{"forgotten_dormant":"<exact-id>","worktree_preserved":true}`; for a live
target it retains the existing unregister result. A missing target remains a
not-found error. Claim and rename conflicts name the destination as dormant and
direct the operator to `ouija dormant show <exact-id>` for inspection and
`ouija unregister <exact-id>` only for intentional release.

At the HTTP boundary those conflicts use `409 Conflict` with
`outcome = "destination_dormant"` and the exact dormant public ID. The CLI
renders the inspection and intentional-forget commands; callers do not have to
parse an unstructured generic occupancy error.

Explicit operator unregister of a live session does not create a tombstone.
Trusted backend SessionEnd and `ReapDead` use dormancy because both represent a
backend identity that may resume; `ouija unregister` is the explicit forget
operation.

A trusted SessionEnd that parks an eligible row returns `{"dormant":"<id>"}`.
An exact but ineligible row retains the existing removed response, while a
stale/superseded callback retains the skipped response. This makes clean-exit
dormancy visible without changing Codex's deliberate no-SessionEnd behavior.

### Canonical project identity and home-managed worktrees

Project handling separates the actual session directory from the canonical
repository identity:

- `SessionMeta.project_dir` stores the normalized actual worktree top used by
  the session, not the common repository root;
- `canonical_project_identity` is the stable comparison/resource-gate key.

Given an absolute, non-root live pane cwd, the daemon resolves these values as
follows:

1. Normalize the physical cwd without requiring nonexistent trailing
   components to exist.
2. Run `git -C <cwd> rev-parse --path-format=absolute --show-toplevel` and
   `git -C <cwd> rev-parse --path-format=absolute --git-common-dir`.
3. When both succeed, the normalized `--show-toplevel` is the actual
   `project_dir`. If the normalized common directory ends in `.git`, its parent
   is `canonical_project_identity`; otherwise the normalized absolute common
   directory itself is the identity.
4. If either Git query fails, preserve the complete normalized cwd as both the
   actual directory and the conservative canonical identity. The fallback
   never strips at a textual `/.ouija/worktrees/` or
   `/.claude/worktrees/` marker, never collapses to `$HOME`, and never guesses a
   repository from `<repo>` or `<session>` path components.

Thus the incident layout
`/home/daniel/.ouija/worktrees/ouija/rootfix`, whose linked-worktree common
directory is `/home/daniel/code/ouija/.git`, stores the actual worktree path in
metadata and `/home/daniel/code/ouija` as canonical identity. If Git
corroboration is unavailable, it conservatively uses the full
`/home/daniel/.ouija/worktrees/ouija/rootfix` path rather than
`/home/daniel`. Existing ordinary repository and in-repository linked-worktree
layouts use the same algorithm. SessionStart performs exact tombstone lookup
before any new-registration home guard.

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
ID, backend pair, actual worktree path, and canonical project identity. It also
rechecks that:

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

Recovered active-context metadata remains parked: the accumulated count and
due bit are preserved, the segment start remains empty, and no dormant interval
is accrued. A carried provisional marker is finalized only as part of the
successful durable recovery described above.

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

The CLI discovers the current adapter's typed backend identity and preserves
each available local signal separately. The raw backend-native ID is sent only
in the JSON body to `POST /api/session-identities/claim` and is never placed in
argv or treated as a public ID.

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

The requested ID is canonical only when
`sanitize_session_id(requested_id) == requested_id` and it is non-empty.
Noncanonical input is rejected with the canonical spelling in the diagnostic;
the CLI does not silently normalize it. Explicit claim never suffixes a name.
The requested ID is the sole name intent; there is no source ID and no rename
alias.

### Required evidence

Self-claim requires all of the following positive evidence:

- exactly one complete adapter identity;
- exactly one current tmux pane resolved by an explicit pane signal or the
  adapter attestation path below;
- a fresh assistant-pane observation for that pane;
- a process matching the named backend adapter;
- a usable, canonical, non-root project path from the live pane;
- no positive pane, environment, marker, or backend observation resolving the
  caller to a different Local owner.

The daemon derives project metadata from the live pane. Caller-supplied project
text is not authoritative.

The coordinator holds pane, backend-session, and project resource gates across
inspection, compare-and-swap, durable persistence, and effect scheduling.

#### Exact pane proof when the tool shell lacks `TMUX_PANE`

Codex and OpenCode tool shells can legitimately lack `TMUX_PANE`; the claim
contract does not require the CLI to guess one. The daemon maintains a
transient `LocalBackendPaneAttestation` keyed by the complete typed backend
pair. It can be established only by:

- a locally installed SessionStart adapter callback carrying an explicit pane
  and the same complete backend pair; or
- the OpenCode readiness callback carrying both explicit `pane` and `cwd`,
  after the daemon verifies that exact backend session through OpenCode's API.

Those callbacks record the attestation before generic name derivation and keep
it when normal registration is deliberately disabled or rejected by the
home-cwd guard. A successful registration/claim consumes or invalidates the
unregistered attestation.

Before recording an attestation, the daemon freshly verifies the exact pane is
an assistant pane running the named backend, derives its actual/canonical
project identity, reads its current Ouija pane marker, and rejects any foreign
live, dormant, or lease owner. An exact tombstone for the same backend pair is
retained for recovery rather than treated as foreign. The attestation stores
the exact pane, backend pair, project identities, independently observed
pane-marker value, and an observation generation. It is in-memory only,
creates no public ID or reservation, is replaced only by a newer observation
of the same exact pair, and is invalidated when the pane/process/project no
longer revalidates or the daemon restarts.

When `TMUX_PANE` is present, the CLI sends that pane plus its independently read
pane-var and environment evidence. When it is absent, the CLI sends
`pane = null`, `pane_var_id = null`, the independently captured `env_id`, and
the complete adapter identity. The claim endpoint may then select a pane only
by an exact, unique attestation for that backend pair. It revalidates the
attested pane/process/project and reads the current pane marker again while
holding resource gates. Any explicit-pane/attestation disagreement, changed
generation, conflicting pane-var/env/marker value, missing attestation,
ambiguous backend-pair attestation, or daemon restart fails closed with a
diagnostic instructing the operator to run the command from a shell with the
exact `TMUX_PANE` or retrigger the trusted adapter callback. It never scans by
project/name, chooses a foreground client, or selects among process matches.

This attestation is corroboration, not identity resurrection. Exact tombstone
recovery still takes precedence, and the final claim transition still proves
that the backend pair, pane, ID, project, and leases are all unowned.

### Shared name resolution

Extend the existing shared registration-name helper with two explicit modes
over one occupancy view containing live and dormant IDs:

- automatic mode retains `sanitize_session_id` plus deterministic suffix
  selection for generic discovery/registration;
- exact mode returns `available`, `same-owner idempotent`, or an occupied
  conflict and never suffixes.

Claim first rejects noncanonical input, then uses exact mode. Rename retains
its existing source/syntax compatibility but uses the same exact destination
mode. Generic registration, claim, and rename therefore cannot disagree about
a live or dormant destination, while an explicitly requested name is never
silently changed.

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

### Dormancy

| Observed state | Result |
|---|---|
| Exact eligible Local owner from reaper or trusted SessionEnd | Close active segment, park one tombstone, remove live routing |
| Exact owner lacks a complete backend pair or safe project identity | Remove without a recoverable tombstone |
| Stale owner, pane, or backend observation | No-op; preserve the current winner |
| Matching lifecycle lease exists | Reject/no-op; lifecycle completion or rollback remains authoritative |
| Active timestamp moves backward | Accrue zero for that segment; preserve prior accumulated time |
| Accumulated time would overflow | Saturate at `u64::MAX` and recompute due |

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
| Requested ID is tombstoned by a different backend pair | Reject as dormant and provide inspection/intentional-unregister remediation |
| Requested ID is noncanonical | Reject and report canonical spelling; never normalize or suffix |
| Caller pane belongs to or is reserved by another owner | Reject |
| Backend pair belongs to or is reserved by another owner | Reject |
| Positive self-ID, environment, marker, or backend evidence identifies a sibling | Reject |
| `TMUX_PANE` is absent and no exact current backend-pair attestation exists | Reject with explicit-pane/adapter-refresh remediation |
| Explicit pane and backend-pair attestation disagree | Reject |
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
| Tombstoned destination occupied | Reject without changing source or tombstone; identify dormant conflict and remediation |
| Source or destination has a lifecycle lease | Reject |
| Same source and destination | Idempotent success for the verified current owner |

## Pure State Transitions and Side Effects

The pure state machine gains these explicit events:

- `DormantOwned { owner, expected_pane, observed_at, source }`, parking one
  exact live Local owner at a daemon observation timestamp;
- consuming one exact tombstone into a recovered Local row;
- claiming one free public ID for a corroborated unregistered Local backend.

None of these events bumps `last_metadata_update`. Dormancy and tombstone
recovery preserve the existing user-facing metadata timestamp; self-claim
creates initial metadata but does not pretend that the user updated role or
bulletin.

The recovery and claim events emit structured success acknowledgements so
callers never infer success from generic persistence effects. They must reject
conflicts before any map removal or insertion. They must not call ordinary
`apply_register`, whose pane-dedup behavior is intentionally unsuitable for
fail-closed recovery and claim.

Reaper and trusted SessionEnd both call the same exact-owner dormancy event.
Explicit `Remove`/operator `unregister` remains separate and intentionally
forgets live or dormant state. The dormancy event reuses one pure
active-context stop helper so stopped, reaped, and clean-exit accounting cannot
drift.

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

Version 1 live rows have only `project_dir`. During hydration, an absolute,
non-root existing directory is passed through the canonical project algorithm
above; a Git failure uses the full normalized path fallback. The derived
identity is written on the next version 2 persistence. A missing/unsafe legacy
directory may remain a live compatibility row, but it is ineligible for
dormancy, recovery, or self-claim corroboration until a live pane supplies a
safe project identity. Version 2 tombstones always carry both the original
actual directory and canonical identity.

Version 2 prevents an older binary from silently loading and later dropping
tombstones. A downgrade to a version-1-only binary fails on the unknown
snapshot version instead of erasing continuity state.

Load-time validation rejects:

- a tombstone whose map key, embedded ID, and prior owner ID disagree;
- incomplete backend pairs or missing/unsafe project identity;
- duplicate public IDs across live and dormant state;
- one backend pair assigned to distinct live, dormant, or lifecycle owners;
- a dormant record with an open active-context segment.

Normalization raises the high-water mark to cover live sessions, tombstones,
restart snapshots, and lease resource owners.

Startup restores tombstones before live-session reconciliation. Abandoned
lease cleanup treats a valid tombstone as a resource sharer and must not delete
its backend session or project directory. Reconciled persistence writes the
tombstone map unchanged. `LocalBackendPaneAttestation` is intentionally absent
from version 2 snapshots; after a daemon restart, a no-`TMUX_PANE` claim must
obtain a fresh trusted adapter callback.

## Test and Model-Check Plan

The already-red regressions remain the first proof, with one required rewrite
before production implementation: the unstaged API tests currently named
`rename_claims_*`, `rename_retries_*`, and
`rename_unregistered_claim_cannot_*` still exercise absent-source rename. They
must be rewritten against the explicit claim endpoint/contract; absent-source
rename stays a strict failure.

- SessionStart after `%802` is reaped and the exact Codex thread appears in
  `%819` restores `rootfix`, its lifecycle metadata, and a newer incarnation;
- trusted Claude SessionEnd parks a complete owner, and SessionStart for the
  same surviving Claude backend identity recovers the prior public ID and
  metadata with a newer incarnation;
- an occupied ordinary rename preserves both source and destination;
- an explicit verified claim creates a requested free ID;
- exact claim retry is idempotent;
- a claimant cannot take an occupied requested ID;
- focused Stateright exploration finds the current occupied-rename overwrite.

Additional pure-state tests cover:

- reaping a complete eligible row creates one tombstone and no live row;
- trusted SessionEnd and reaping produce equivalent exact-owner dormant state;
- stale SessionEnd cannot park a replacement owner;
- rows with incomplete identity or unusable project metadata do not create a
  recoverable tombstone;
- dormancy at an observation timestamp closes an open active segment using
  non-negative/saturating arithmetic, recomputes due, and persists it parked;
- dormant time is not accrued and recovery leaves the segment closed until a
  new exact-owner Active signal;
- provisional accounting is preserved while dormant, recovery finalizes it
  only after its durable commit, and failed recovery leaves it provisional;
- exact recovery consumes one tombstone and preserves every persistent
  lifecycle field;
- recovered incarnation is strictly newer;
- recovery retry does not allocate again;
- explicit dormant unregister releases the reservation without worktree
  cleanup;
- live/tombstone destination, foreign pane, foreign backend pair, conflicting
  project/ID lease, stale tombstone owner, and persistence rollback all fail
  without mutation;
- shared automatic name resolution suffixes across live and dormant occupancy,
  while exact claim/rename mode never suffixes or overwrites.

API and CLI tests cover:

- `ouija claim <requested-id>` parsing and request construction;
- canonical requested-ID acceptance plus noncanonical rejection with no
  suffixing;
- missing or incomplete adapter identity;
- missing/non-assistant pane, backend-process mismatch, project mismatch, and
  conflicting positive caller evidence;
- no-`TMUX_PANE` claim through an exact Codex/OpenCode backend-pair
  attestation, plus missing/stale/ambiguous/conflicting-attestation failures;
- recovery precedence when the requested ID differs from the tombstone ID;
- exact idempotent retry;
- Local-only routing with no Nostr command path;
- dormant list/show redaction and exact dormant unregister result;
- dormant claim/rename diagnostics with safe inspection and cleanup commands;
- response status and public ID for claimed, recovered, current, conflict, and
  persistence-failure outcomes.

Project-identity tests cover normal repositories, in-repository linked
worktrees, non-`.git` absolute common directories, Git-query failure fallback,
and the exact rootfix layout:
`/home/daniel/.ouija/worktrees/ouija/rootfix` remains the metadata worktree
while `/home/daniel/code/ouija` is the Git-corroborated canonical identity. A
fallback test proves the same input never becomes `/home/daniel`.

Persistence tests cover version-1 migration, version-2 round-trip, high-water
normalization, validation of malformed/colliding tombstones, preservation
during abandoned-lease reconciliation, dormancy timestamp/source and parked
accounting round-trip, omission of transient attestations, and rollback on
write failure.

Stateright properties cover:

- no public destination is overwritten by rename, claim, or recovery;
- each complete backend pair has at most one distinct Local owner across live
  sessions, tombstones, and lifecycle claims;
- recovery consumes a tombstone at most once;
- every recovered incarnation is greater than the tombstone incarnation;
- stale reap/recovery results cannot remove or replace a newer winner;
- reaping, trusted SessionEnd, and dormant cleanup never emit worktree
  deletion;
- dormant active-context segments are always parked and accumulated active time
  never decreases across a dormancy/recovery sequence.

Focused tests run during each red-green cycle. Final verification runs:

```text
cargo test
cargo test model_check_bfs -- --ignored --nocapture
cargo clippy --all-targets --all-features
tests/e2e/run-e2e.sh local
```

`tests/e2e/run-e2e.sh` deliberately runs the daemon and tmux server inside
Docker, so it does not mutate the live host daemon or host tmux state. Add a
targeted local-suite scenario using isolated fake assistant panes and the real
daemon/HTTP/CLI boundary. It must claim a canonical free ID, park that complete
owner through trusted SessionEnd (and, where the fixture permits, reaping),
recover the same backend pair in a replacement pane with the same ID and newer
incarnation, and prove an occupied rename preserves both rows. The fixture must
also inspect and explicitly unregister a dormant ID, asserting the worktree is
preserved. Unit/model tests remain authoritative for races and persistence
rollback that the process-level scenario cannot deterministically schedule.

## Out of Scope

- Inferring public identity from project basename, worktree slug, or backend
  native ID alone;
- changing Nostr sender authentication or remote-session naming;
- converting arbitrary missing-source rename requests into claims;
- TTL, automatic garbage collection, or heuristic reassignment of tombstones;
- selecting a claim pane by project/name scans, foreground-client inference, or
  process-match guessing;
- reviving incomplete legacy backend rows;
- changing managed-launch credential rules or the existing operator
  `recover-backend-identity` path;
- deleting preserved worktrees during reap or dormant reservation cleanup.
