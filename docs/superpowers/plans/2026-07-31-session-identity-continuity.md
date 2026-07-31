# Session Identity Continuity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` (recommended) or
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve a Local assistant's public Ouija identity and lifecycle
metadata across pane loss or trusted backend exit, add an explicit fail-closed
`ouija claim <requested-id>` path for genuinely unregistered assistants, and
prevent rename/registration conflicts from overwriting live or dormant owners.

**Architecture:** Extend the pure `DaemonState::apply(Event)` state machine
with durable dormancy, exact recovery, exact claim, and collision-safe rename
transitions. Keep physical corroboration, transient backend/pane attestations,
resource gates, persistence rollback, and effect execution in `AppState`.
Expose only Local HTTP/CLI surfaces; Nostr ingress remains unchanged.

**Tech Stack:** Rust, Tokio, Axum, Clap, serde/JSON persistence, tmux process
inspection, Git CLI project corroboration, Stateright, TypeScript OpenCode
plugin, Bash/Docker E2E.

## Global Constraints

- The approved contract is
  `docs/superpowers/specs/2026-07-31-session-identity-continuity-design.md`
  at commit `d8ad5aa`. If code evidence contradicts that contract, stop and
  ask parent `ouija`; do not silently change the contract.
- Work only in the assigned worktree. Never restart or install the live daemon,
  touch a live Ouija session, push, publish, or open a PR.
- Preserve the Local/Nostr trust split. Do not add claim/recovery commands to
  `WireMessage`, Nostr ingress, or remote transport routing.
- All state mutation starts in `DaemonState::apply`. Coordinators clone the
  protocol state, apply the pure event, persist the complete candidate state,
  roll back on failure, then execute effects.
- Resource-gated operations lock public ID, pane, complete backend pair, and
  canonical project identity. They never evict an incumbent.
- New tmux effects must retain the `cfg!(test)` host-isolation guard.
- Do not silently normalize or suffix explicit claim IDs.
- Do not delete worktrees during reaping, trusted SessionEnd, recovery, or
  dormant unregister.
- Run the exact focused red and green command in each task before committing.
  If an unrelated pre-existing red regression blocks a focused command, record
  the evidence and ask the parent instead of weakening the test.

## Interface Map

### Pure protocol and durable records

**Files:** `src/daemon_protocol.rs`, `src/persistence.rs`

Add these durable types and fields in `daemon_protocol.rs`:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DormancySource {
    Reaped,
    TrustedSessionEnd,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct DormantSession {
    pub id: String,
    pub prior_owner: ResourceOwner,
    pub metadata: SessionMeta,
    pub canonical_project_identity: String,
    pub dormant_at: i64,
    pub source: DormancySource,
}

pub struct DaemonState {
    // existing fields...
    pub dormant_sessions: BTreeMap<String, DormantSession>,
}
```

`SessionMeta` gains
`canonical_project_identity: Option<String>`. Its persisted mirror
`state::SessionMetadata` gains the same field. All explicit conversions in
`state.rs` and `main.rs` must map it; defaults remain `None` for legacy rows.

Add pure events:

```rust
Event::DormantOwned {
    owner: ResourceOwner,
    expected_pane: Option<String>,
    observed_at: i64,
    source: DormancySource,
}
Event::RecoverDormantSession {
    dormant_owner: ResourceOwner,
    pane: String,
    backend: String,
    backend_session_id: String,
    project_dir: String,
    canonical_project_identity: String,
}
Event::ClaimLocalSession {
    requested_id: String,
    pane: String,
    backend: String,
    backend_session_id: String,
    project_dir: String,
    canonical_project_identity: String,
}
```

Add structured acknowledgements:

```rust
pub enum LocalClaimDisposition { Created, Current }

Effect::DormancyApplied {
    id: String,
    prior_owner: ResourceOwner,
    tombstoned: bool,
}
Effect::DormantRecovered { owner: ResourceOwner }
Effect::LocalClaimed {
    owner: ResourceOwner,
    disposition: LocalClaimDisposition,
}
Effect::DormantForgotten { id: String }
```

`RenameFailed` must carry a closed reason enum so API code can distinguish
`DestinationDormant` from other conflicts without parsing text:

```rust
pub enum RenameFailureKind {
    SourceMissing,
    SourceNotLocal,
    SourceLease,
    DestinationLease,
    DestinationLive,
    DestinationDormant,
    InvalidDestination,
}
```

### Project identity and shared name resolution

**Files:** `src/project_identity.rs` (new), `src/main.rs`, `src/state.rs`,
`src/daemon_protocol.rs`, `src/hooks.rs`, `src/api.rs`,
`src/backend/mod.rs`, `src/backend/claude_code.rs`,
`src/backend/opencode.rs`

Create one synchronous project resolver:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectIdentity {
    pub project_dir: String,
    pub canonical_repository: String,
}

pub(crate) fn resolve_project_identity(
    path: &str,
) -> Result<ProjectIdentity, ProjectIdentityError>;
```

It performs the approved absolute-path normalization plus
`git rev-parse --show-toplevel/--git-common-dir` algorithm. Async paths call it
through `tokio::task::spawn_blocking`; unit tests call it directly. Remove the
textual worktree truncation from `state::resolve_project_root`. The actual
worktree remains `SessionMeta.project_dir`; only
`canonical_project_identity` is used as the stable comparison/resource key.

Move `resolve_unique_session_id` into the pure `daemon_protocol.rs` layer as
one helper over a combined live and dormant occupancy view. `state.rs` calls
this exported pure helper; `daemon_protocol.rs` uses it directly for claim and
rename, avoiding a core-to-coordinator dependency:

```rust
pub(crate) enum NameResolutionMode<'a> {
    Automatic { target_pane: Option<&'a str> },
    Exact { same_owner: Option<&'a ResourceOwner> },
}

pub(crate) enum NameResolution {
    Available(String),
    Idempotent(String),
    Occupied { id: String, dormant: bool },
}

pub(crate) fn resolve_session_id(
    sessions: &BTreeMap<String, SessionEntry>,
    dormant: &BTreeMap<String, DormantSession>,
    requested: &str,
    mode: NameResolutionMode<'_>,
) -> NameResolution;
```

Automatic mode sanitizes and suffixes. Exact mode never suffixes. Claim first
requires nonempty input and `sanitize_session_id(input) == input`.

### Transient Local corroboration

**Files:** `src/state.rs`, `src/hooks.rs`, `src/api.rs`,
`src/backend/mod.rs`, `src/backend/opencode.rs`,
`opencode-plugin/ouija.ts`

Add an in-memory, non-serialized attestation:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocalBackendPaneAttestation {
    pub identity: BackendSessionIdentity,
    pub pane: String,
    pub project: ProjectIdentity,
    pub pane_var_id: Option<String>,
    pub generation: u64,
}

type BackendIdentityKey = (String, String);
```

`AppState` gains
`local_backend_pane_attestations:
RwLock<BTreeMap<BackendIdentityKey, LocalBackendPaneAttestationState>>` and a
monotonic in-memory generation counter, where:

```rust
pub(crate) enum LocalBackendPaneAttestationState {
    Unique(LocalBackendPaneAttestation),
    Ambiguous {
        panes: BTreeSet<String>,
        generation: u64,
    },
}
```

A new observation replaces an invalid/dead prior observation. If the prior
pane still revalidates and the same pair appears in a different live pane, the
slot becomes `Ambiguous`; it cannot authorize claim until a later trusted
callback observes one surviving pane and the others fail revalidation.
Initialize the map and counter in both production and test constructors.

The installed Codex SessionStart callback and OpenCode readiness callback may
record attestations only after fresh pane, backend process, project, marker,
owner, dormant-pair, and lease checks. Recording happens before the existing
auto-registration/home-cwd exits. Claim revalidates the same evidence while
holding resource gates.

OpenCode's documented `shell.env` hook supplies the exact session-local
identity to tool shells:

```ts
"shell.env": async (input, output) => {
  output.env.OPENCODE_SESSION_ID = input.sessionID
},
```

`OpenCode::caller_session_id()` reads nonempty `OPENCODE_SESSION_ID`. The
existing backend registry then preserves its fail-closed “exactly one adapter
identity” behavior. This is the concrete no-`TMUX_PANE` proof path; no project,
name, process-scan, or foreground-client inference is permitted.

### Local API and CLI

**Files:** `src/state.rs`, `src/api.rs`, `src/server.rs`, `src/main.rs`,
`skills/ouija/SKILL.md`

Define the evidence contract in `state.rs` so the coordinator does not depend
on the HTTP module:

```rust
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalClaimEvidence {
    pub pane: Option<String>,
    pub pane_var_id: Option<String>,
    pub env_id: Option<String>,
    pub backend_identity: BackendSessionIdentity,
}
```

Use it from the API request:

```rust
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalClaimRequest {
    pub requested_id: String,
    pub caller: LocalClaimEvidence,
}

pub async fn claim_local_identity(
    State(state): State<SharedState>,
    Json(body): Json<LocalClaimRequest>,
) -> (StatusCode, Json<Value>);
```

Routes:

```text
POST /api/session-identities/claim
GET  /api/session-identities/dormant
GET  /api/session-identities/dormant/{id}
```

Clap additions:

```rust
Command::Claim { requested_id: String }
Command::Dormant { command: DormantCommand }

enum DormantCommand {
    List,
    Show { id: String },
}
```

`ouija claim` calls `BackendRegistry::caller_session_identity()`, separately
reads `TMUX_PANE`, the pane's `@ouija_session`, and `OUIJA_SESSION_ID`, then
sends the dedicated JSON contract. It never places the backend-native ID in
argv. `ouija unregister` remains the sole cleanup command and distinguishes
live removal from dormant forget.

### Coordinator operations

**File:** `src/state.rs`

Add closed outcomes, rather than string matching:

```rust
pub(crate) enum LocalClaimOutcome {
    Claimed(ResourceOwner),
    Current(ResourceOwner),
    Recovered(ResourceOwner),
    InvalidId { requested: String, canonical: String },
    DestinationLive { id: String },
    DestinationDormant { id: String },
    AlreadyRegistered { id: String },
    EvidenceConflict(&'static str),
    ResourceConflict(&'static str),
    PersistenceFailed(String),
}

pub(crate) async fn claim_local_identity(
    self: &Arc<Self>,
    requested_id: &str,
    evidence: &LocalClaimEvidence,
) -> LocalClaimOutcome;
```

Use one persistence-atomic coordinator for every live-to-dormant transition:

```rust
pub(crate) enum DormancyOutcome {
    Dormant { id: String, prior_owner: ResourceOwner },
    Removed { id: String, prior_owner: ResourceOwner },
    Superseded,
    LeaseConflict,
    PersistenceFailed(String),
}

pub(crate) async fn dormant_owned(
    self: &Arc<Self>,
    owner: ResourceOwner,
    expected_pane: Option<&str>,
    observed_at: i64,
    source: DormancySource,
) -> DormancyOutcome;
```

`dormant_owned` acquires the owner ID, pane, backend-pair, and canonical-project
gates; clones the protocol; applies exactly one `Event::DormantOwned`; persists
the complete candidate; installs the candidate only after persistence succeeds;
then executes its non-`PersistSessions` effects. `Dormant` means the pure
transition replaced the live row with a tombstone. `Removed` means that same
pure transition removed an ineligible exact row. On persistence failure the
original live row remains authoritative, no tombstone is installed, and no
pane/agent/announcement effect runs.

Implement dormant recovery as a separate resource-gated coordinator reused by
SessionStart and claim:

```rust
pub(crate) async fn recover_dormant_session(
    self: &Arc<Self>,
    identity: &BackendSessionIdentity,
    pane: &str,
    project: &ProjectIdentity,
) -> DormantRecoveryOutcome;
```

It compares the exact tombstone owner, applies the pure event to a candidate,
persists, rolls back on error, and only then executes effects.

### Persistence and startup

**Files:** `src/persistence.rs`, `src/main.rs`, `src/state.rs`

Set `SESSION_STATE_VERSION` to 2 and add
`#[serde(default)] dormant_sessions: BTreeMap<String, DormantSession>` to the
versioned snapshot. Change the constructor to:

```rust
PersistedLifecycleState::new(
    sessions: Vec<PersistedSession>,
    dormant_sessions: BTreeMap<String, DormantSession>,
    incarnation_high_water: SessionIncarnation,
    lifecycle_leases: BTreeMap<String, LifecycleLease>,
) -> Self
```

Load legacy arrays, migrate version 1 to empty dormancy, accept version 2, and
reject every other version. Validation covers key/embedded-owner agreement,
complete pairs, safe parked projects, live/dormant ID and backend-pair
uniqueness, and closed active segments. High-water normalization includes live,
dormant, restart-snapshot, and lease owners.

Startup restores dormant rows before abandoned-lease reconciliation. Its
backend/worktree “sharer” checks include tombstones. A dead eligible persisted
live row becomes dormant at the startup observation time; an ineligible row is
forgotten. Reconciliation persists schema v2 atomically.

---

## Task 1: Replace the Stale Absent-Source Rename Claim Tests

**Files:**

- Modify: `src/state.rs` for the shared `LocalClaimEvidence` contract
- Modify: `src/api.rs` tests near the existing `rename_claims_*` regressions
- Modify: `src/api.rs` request/handler declarations
- Modify: `src/server.rs` route table

**Consumes:** Approved explicit-claim JSON contract.

**Produces:** Three endpoint tests that no longer grant authority through a
missing rename source, plus a compileable `501 Not Implemented` claim endpoint
stub so the tests fail for the intended behavioral reason.

- [ ] Rename and rewrite the three unstaged tests as:
  `claim_creates_requested_free_id_for_verified_unregistered_backend`,
  `claim_retry_by_same_owner_is_idempotent`, and
  `claim_cannot_take_occupied_destination`.
- [ ] Build `LocalClaimRequest`/`LocalClaimEvidence` directly in those tests.
  Keep the current cached-pane fixtures and backend pairs; call
  `claim_local_identity`, not `rename`.
- [ ] Add `rename_missing_source_never_claims` to assert `404`, no new session,
  and unchanged occupied/live state.
- [ ] Run the red test:

  ```bash
  cargo test api::tests::claim_creates_requested_free_id_for_verified_unregistered_backend -- --exact
  ```

  Expected: compile failure because the claim request/handler does not exist.

- [ ] Add only the request structs, Local-only server route, and a handler that
  returns:

  ```rust
  (
      StatusCode::NOT_IMPLEMENTED,
      Json(json!({"outcome": "not_implemented"})),
  )
  ```

  Define `LocalClaimEvidence` in `state.rs` exactly as mapped above; define
  only `LocalClaimRequest` and the HTTP handler in `api.rs`.

- [ ] Run the executable red contract:

  ```bash
  cargo test api::tests::claim_ -- --nocapture
  ```

  Expected: the three claim tests fail on HTTP status `501`; the missing-source
  rename regression passes.

- [ ] Commit the contract tests and temporary stub:

  ```bash
  git add src/state.rs src/api.rs src/server.rs
  git commit -m "test: pin explicit local claim contract"
  ```

## Task 2: Implement Canonical Project Identity

**Files:**

- Create: `src/project_identity.rs`
- Modify: `src/main.rs`
- Modify: `src/state.rs`
- Modify: `src/hooks.rs`
- Modify: `src/api.rs`
- Modify: `src/backend/mod.rs`
- Modify: `src/backend/claude_code.rs`
- Modify: `src/backend/opencode.rs`
- Test: inline tests in `src/project_identity.rs`

**Consumes:** Absolute pane cwd.

**Produces:** `ProjectIdentity` and `canonical_project_identity` metadata
wiring used by dormancy, resource gates, recovery, and claim.

- [ ] Add red project tests for a normal repo, linked worktree, non-`.git`
  common directory, Git failure, and the exact rootfix-style layout. Build the
  fixture under a temp home-shaped path but assert the concrete invariant:

  ```rust
  assert_eq!(identity.project_dir, worktree.display().to_string());
  assert_eq!(identity.canonical_repository, repository.display().to_string());
  assert_ne!(identity.project_dir, home.display().to_string());
  ```

- [ ] Run red:

  ```bash
  cargo test project_identity::tests -- --nocapture
  ```

  Expected: unresolved module/helper failures.

- [ ] Implement `project_identity.rs`, register `mod project_identity`, and
  replace textual `resolve_project_root` call sites. Preserve the full
  normalized cwd on either Git-query failure.
- [ ] Remove the unused per-backend `CodingAssistant::resolve_project_root`
  hook and its heuristic-only tests so no alternate worktree truncation path
  remains.
- [ ] Add `canonical_project_identity` to `SessionMeta` and
  `SessionMetadata`, updating every explicit conversion and test constructor.
- [ ] Run green:

  ```bash
  cargo test project_identity::tests -- --nocapture
  ```

- [ ] Commit:

  ```bash
  git add src/project_identity.rs src/main.rs src/state.rs src/hooks.rs src/api.rs src/backend/mod.rs src/backend/claude_code.rs src/backend/opencode.rs
  git commit -m "feat: derive canonical repository identity"
  ```

## Task 3: Add Shared Naming and Pure Dormancy, Recovery, Claim, and Safe Rename

**Files:**

- Modify: `src/daemon_protocol.rs`
- Modify: `src/state.rs` effect execution and test constructors
- Test: inline protocol tests in `src/daemon_protocol.rs`

**Consumes:** Exact current owner/tombstone owner and already-corroborated
resource fields.

**Produces:** Durable protocol records/events/effects with no I/O.

- [ ] Add red name tests: automatic mode suffixes across both maps; exact mode
  returns live occupied, dormant occupied, same-owner idempotent, and never
  suffixes. Prefix their function names with `resolve_session_id_`.
- [ ] Add red tests for:
  eligible reap dormancy; equivalent trusted SessionEnd dormancy; stale owner
  no-op; incomplete-pair/ineligible-project removal; lease rejection; active
  segment closure under normal/backward/overflow time; due monotonicity;
  provisional preservation; recovery parked state and provisional
  finalization; metadata preservation; strictly newer incarnation; retry;
  claim creation/idempotency; every live/dormant ID/pane/pair/project/lease
  conflict; and dormant unregister without cleanup effect. Prefix dormancy
  functions with `dormant_` and pure-claim functions with `claim_`.
- [ ] Keep and strengthen the existing red occupied-rename regression: compare
  the complete source and destination entries before/after. Add dormant
  destination and same-ID idempotency cases.
- [ ] Run red:

  ```bash
  cargo test daemon_protocol::tests::resolve_session_id -- --nocapture
  cargo test daemon_protocol::tests::dormant_ -- --nocapture
  cargo test daemon_protocol::tests::claim_ -- --nocapture
  cargo test daemon_protocol::tests::rename_rejects_occupied -- --nocapture
  ```

  Expected: missing event/type failures and the current occupied rename
  overwrite failure.

- [ ] Extract:

  ```rust
  fn close_active_context_segment(metadata: &mut SessionMeta, observed_at: i64)
  ```

  Reuse it from both `apply_active_context_stopped` and `DormantOwned`. Use
  nonnegative `i128`, `u64::try_from(...).unwrap_or(u64::MAX)`, saturating add,
  and the existing monotonic due semantics.
- [ ] Implement the combined-occupancy `resolve_session_id` helper in
  `daemon_protocol.rs` before the events that consume it. Claim and rename use
  `Exact`; the scanner switches to `Automatic` in Task 10.
- [ ] Implement `DormantOwned`, `RecoverDormantSession`, and
  `ClaimLocalSession` as fail-closed pure compare-and-swap transitions.
  Sanitize transient fields in the dormant copy. Never call `apply_register`.
  None of these events changes `last_metadata_update`.
- [ ] Extend `Remove` so exact operator unregister forgets a dormant row and
  emits `DormantForgotten`, but never a worktree cleanup effect.
- [ ] Harden rename by resolving destination before removing source. Return
  `RenameFailureKind`; preserve both maps on every failure.
- [ ] Update exhaustive `Event`/`Effect` matches and test-only effect sinks.
- [ ] Run green:

  ```bash
  cargo test daemon_protocol::tests::resolve_session_id -- --nocapture
  cargo test daemon_protocol::tests::dormant_ -- --nocapture
  cargo test daemon_protocol::tests::claim_ -- --nocapture
  cargo test daemon_protocol::tests::rename_ -- --nocapture
  ```

- [ ] Commit:

  ```bash
  git add src/daemon_protocol.rs src/state.rs
  git commit -m "feat: model durable local identity dormancy"
  ```

## Task 4: Migrate Persistence to Version 2

**Files:**

- Modify: `src/persistence.rs`
- Modify: `src/state.rs`
- Modify: `src/main.rs`
- Test: inline persistence and startup tests

**Consumes:** Complete live state, dormant map, monotonic high-water value,
lifecycle leases.

**Produces:** Backward-compatible v2 snapshots and fail-closed validation.

- [ ] Add red fixtures/tests for legacy arrays, v1-to-empty-dormant migration,
  v2 round-trip, unknown-version refusal, monotonic high-water value
  normalization, malformed key/embedded-owner records, incomplete pairs,
  unsafe/open-segment tombstones, live/dormant ID collisions, pair collisions
  across live/dormant/leases, and timestamp/source/parked-accounting round-trip.
- [ ] Add startup red tests proving already-persisted dormant rows restore
  before reconciliation and their backend/worktree ownership suppresses
  abandoned cleanup.
- [ ] Run red:

  ```bash
  cargo test persistence::tests -- --nocapture
  cargo test tests::restore_persisted -- --nocapture
  ```

- [ ] Set schema version 2, add the serde-defaulted map, implement the
  four-argument constructor, and explicitly accept only legacy, v1, and v2.
- [ ] Normalize/validate against one combined ownership index. Raise the single
  monotonic incarnation high-water value to include dormant prior owners.
- [ ] Update every `PersistedLifecycleState::new` and
  `persist_sessions_from` call. Include dormancy in `persist_protocol_state`.
- [ ] Restore dormancy before abandoned leases. Derive missing v1 canonical
  identity from safe existing paths; never invent it for missing/unsafe paths.
- [ ] Run green:

  ```bash
  cargo test persistence::tests -- --nocapture
  cargo test tests::restore_persisted -- --nocapture
  ```

- [ ] Commit:

  ```bash
  git add src/persistence.rs src/state.rs src/main.rs
  git commit -m "feat: persist dormant identities in schema v2"
  ```

## Task 5: Coordinate Atomic Dormant Recovery and Reaping

**Files:**

- Modify: `src/state.rs`
- Modify: `src/main.rs`
- Modify: `src/hooks.rs`
- Test: inline state, main, and hook tests

**Consumes:** Complete backend identity, replacement pane, canonical project,
exact tombstone, and resource gates.

**Produces:** Atomic persistence-backed recovery reused by SessionStart and
claim; one atomic dormancy coordinator reused by startup, reaper, and trusted
SessionEnd.

- [ ] Add red `AppState::dormant_owned` tests for eligible parking, ineligible
  exact removal, stale owner, lease conflict, and persistence failure. Both
  eligible and ineligible persistence failures must preserve the original live
  row, install no tombstone, and execute no external effect. Prefix these
  functions with `dormant_owned_`.
- [ ] Add a startup red test proving a dead eligible persisted live row is
  inserted as live authority and then parked through
  `AppState::dormant_owned`, with the startup observation timestamp. Name it
  `restore_dead_live_row_uses_atomic_dormancy_coordinator`.
- [ ] Add a reaper red test named
  `reaper_uses_atomic_dormancy_coordinator` that observes the same persisted
  outcome through the sweep boundary.
- [ ] Add red state tests for recovery success, persistence failure rollback,
  stale tombstone, changed project, live prior ID, foreign/reserved pane,
  foreign/reserved pair, project lease, retry, and recovery precedence before
  generic registration/home guard. Prefix these functions with
  `recover_dormant_`.
- [ ] Preserve the existing unstaged `%802`/`%819` rootfix Codex regression and
  expand it to assert arbitrary public ID, complete lifecycle metadata, parked
  accounting, and strictly newer incarnation.
- [ ] Run red:

  ```bash
  cargo test hooks::tests::resumed_backend_recovers_reaped_public_id_and_lifecycle_metadata -- --exact --nocapture
  cargo test state::tests::dormant_owned -- --nocapture
  cargo test state::tests::recover_dormant -- --nocapture
  cargo test tests::restore_dead_live_row_uses_atomic_dormancy_coordinator -- --exact --nocapture
  cargo test tests::reaper_uses_atomic_dormancy_coordinator -- --exact --nocapture
  ```

- [ ] Implement `AppState::dormant_owned` with the coordinator contract mapped
  above. Persist the candidate before swapping protocol state or executing any
  non-persistence effect.
- [ ] Implement `AppState::recover_dormant_session` using the existing
  `recover_backend_identity` pattern: collect candidate, acquire all event
  resource gates, freshly inspect pane/backend/project/marker, clone protocol,
  apply pure event, persist candidate, replace on success, and execute effects.
- [ ] In `session_start_inner`, perform live-pair reclaim first, then dormant
  lookup/recovery before basename derivation and home guard. A matching
  tombstone that fails corroboration returns a closed failure and never falls
  through.
- [ ] Capture reaper `observed_at` when pane death is confirmed. Apply
  dormancy only through `AppState::dormant_owned`. Startup dead-row
  reconciliation uses the same coordinator after restoring the live row. Do
  not apply a second removal when the outcome is `Removed`; the one pure
  transition already removed and persisted the ineligible exact row.
- [ ] Ensure recovery effects set pane markers, start the session agent, and
  re-announce only after persistence success.
- [ ] Run green:

  ```bash
  cargo test hooks::tests::resumed_backend_recovers_reaped_public_id_and_lifecycle_metadata -- --exact --nocapture
  cargo test state::tests::dormant_owned -- --nocapture
  cargo test state::tests::recover_dormant -- --nocapture
  cargo test tests::restore_dead_live_row_uses_atomic_dormancy_coordinator -- --exact --nocapture
  cargo test tests::reaper_uses_atomic_dormancy_coordinator -- --exact --nocapture
  ```

- [ ] Commit:

  ```bash
  git add src/state.rs src/main.rs src/hooks.rs
  git commit -m "feat: recover dormant identity after pane replacement"
  ```

## Task 6: Park Trusted SessionEnd Through the Same Transition

**Files:**

- Modify: `src/hooks.rs`
- Test: inline hook tests

**Consumes:** Trusted SessionEnd resolved to exact incarnation plus pane or
backend pair, and `AppState::dormant_owned`.

**Produces:** Clean Claude exit dormancy with visible response semantics.

- [ ] Add red tests for eligible clean Claude exit, clean-exit/replacement-pane
  recovery, incomplete row removal, stale/superseded callback no-op, lease
  conflict, active-segment accounting equivalence with reaping, and persistence
  failure. The failure case asserts the live row remains, no tombstone appears,
  no external dormancy effect executes, and the hook does not report success.
- [ ] Run red:

  ```bash
  cargo test hooks::tests::session_end_ -- --nocapture
  ```

- [ ] Replace `Event::RemoveOwned` in `session_end_inner` with
  `AppState::dormant_owned(..., DormancySource::TrustedSessionEnd)`. The hook
  never calls `apply_and_execute(Event::DormantOwned)` directly.
- [ ] Map effects exactly:
  eligible park to `{"dormant":"<id>"}`, ineligible exact row to the existing
  removed response, stale owner to the existing skipped response, and
  `PersistenceFailed` to an internal-error response without lifecycle success.
- [ ] Confirm Codex remains no-SessionEnd and explicit `ouija unregister`
  remains intentional forget.
- [ ] Run green:

  ```bash
  cargo test hooks::tests::session_end_ -- --nocapture
  ```

- [ ] Commit:

  ```bash
  git add src/hooks.rs
  git commit -m "feat: park trusted session end identities"
  ```

## Task 7: Establish and Revalidate Backend/Pane Attestations

**Files:**

- Modify: `src/state.rs`
- Modify: `src/hooks.rs`
- Modify: `src/api.rs`
- Modify: `src/backend/opencode.rs`
- Modify: `opencode-plugin/ouija.ts`
- Test: inline Rust tests and embedded-plugin string tests

**Consumes:** Trusted explicit-pane adapter callbacks and complete typed backend
pairs.

**Produces:** Transient exact attestations and an OpenCode tool-shell backend
identity signal.

- [ ] Add red tests for SessionStart attestation retention when auto
  registration/home guard rejects, OpenCode readiness retention when
  `auto_register=false`, replacement by newer same-pair observation, and
  rejection/invalidation for non-assistant pane, backend mismatch, foreign
  marker/owner, changed project, lease, stale pane, and daemon restart.
  Prefix the state functions with `local_backend_pane_attestation_` and the
  readiness function with `ready_records_attestation_`.
- [ ] Add red backend/plugin tests proving `OpenCode::caller_session_id` reads
  only nonempty `OPENCODE_SESSION_ID` and embedded TypeScript defines
  `shell.env` with `output.env.OPENCODE_SESSION_ID = input.sessionID`. Prefix
  the focused backend functions with `caller_session_`.
- [ ] Run red:

  ```bash
  cargo test state::tests::local_backend_pane_attestation -- --nocapture
  cargo test api::tests::ready_records_attestation -- --nocapture
  cargo test backend::opencode::tests::caller_session -- --nocapture
  ```

- [ ] Add the attestation map/generation to both `AppState` constructors.
  Keep it absent from serde and persistence.
- [ ] Implement record/revalidate/consume methods keyed only by the complete
  `(backend, session_id)` pair. Read current pane marker independently.
- [ ] Record from explicit Codex/Claude SessionStart and verified OpenCode
  readiness before generic registration exits. Do not record from scan-by-dir.
- [ ] Add the OpenCode `shell.env` hook and `caller_session_id` implementation.
  Verify two positive backend env signals still make
  `BackendRegistry::caller_session_identity()` return `None`.
- [ ] Run green:

  ```bash
  cargo test state::tests::local_backend_pane_attestation -- --nocapture
  cargo test api::tests::ready_records_attestation -- --nocapture
  cargo test backend::opencode::tests -- --nocapture
  ```

- [ ] Commit:

  ```bash
  git add src/state.rs src/hooks.rs src/api.rs src/backend/opencode.rs opencode-plugin/ouija.ts
  git commit -m "feat: attest local backend pane identity"
  ```

## Task 8: Implement the Claim Coordinator, Endpoint, and CLI

**Files:**

- Modify: `src/state.rs`
- Modify: `src/api.rs`
- Modify: `src/main.rs`
- Test: inline state, API, and CLI tests

**Consumes:** `LocalClaimEvidence`, exact attestation/explicit pane, shared
exact-name resolver, pure claim/recovery events.

**Produces:** Working `ouija claim <requested-id>` with structured outcomes.

- [ ] Expand Task 1's red tests into the full matrix: canonical/noncanonical
  IDs; incomplete/ambiguous adapter identity; missing/non-assistant pane;
  backend mismatch; project mismatch; conflicting pane/env/marker evidence;
  missing/stale/ambiguous attestation; explicit-pane/attestation disagreement;
  every ID/pane/pair/project lease; live/dormant destination; persistence
  rollback; different requested ID with recovery precedence; and exact retry.
- [ ] Add CLI parse/request tests. Use scoped environment mutation helpers
  already present in backend tests; serialize env-sensitive tests where
  required. Prefix coordinator functions with `claim_local_identity_`, API
  functions with `claim_`, and root `main.rs` CLI functions with `claim_`.
- [ ] Run red:

  ```bash
  cargo test api::tests::claim_ -- --nocapture
  cargo test state::tests::claim_local_identity -- --nocapture
  cargo test tests::claim_ -- --nocapture
  ```

  Expected: Task 1 tests still fail on the `501` stub.

- [ ] Implement `AppState::claim_local_identity`. Resolve an explicit pane or
  exact unique attestation; acquire ID/pane/pair/project gates; revalidate all
  positive evidence; give exact tombstone recovery precedence; then apply and
  durably persist `ClaimLocalSession`.
- [ ] Replace the `501` handler with exhaustive `LocalClaimOutcome` mapping:
  success/current/recovered `200`, invalid/evidence `400`, Local authority
  conflicts `403`, live/dormant/resource/lease conflicts `409`, and persistence
  failure `500`. Dormant conflict returns
  `outcome = "destination_dormant"` and exact remediation commands.
- [ ] Add Clap `Claim`. Gather pane, pane var, `OUIJA_SESSION_ID`, and typed
  backend identity separately; never collapse disagreements before the daemon.
  Send the backend-native ID only in JSON.
- [ ] Confirm no Nostr/Wire enum or route changes.
- [ ] Run green:

  ```bash
  cargo test api::tests::claim_ -- --nocapture
  cargo test state::tests::claim_local_identity -- --nocapture
  cargo test tests::claim_ -- --nocapture
  ```

- [ ] Commit:

  ```bash
  git add src/state.rs src/api.rs src/main.rs
  git commit -m "feat: claim an exact local session identity"
  ```

## Task 9: Expose Dormant Inspection and Intentional Cleanup

**Files:**

- Modify: `src/api.rs`
- Modify: `src/server.rs`
- Modify: `src/main.rs`
- Modify: `skills/ouija/SKILL.md`
- Test: inline API/CLI tests

**Consumes:** Durable dormant map and explicit operator target.

**Produces:** Local-only list/show and exact dormant unregister UX.

- [ ] Add red tests for list/show fields, opaque backend-session fingerprint,
  absence of credentials/reservations/pending/routability, encoded IDs,
  missing target, dormant unregister result, worktree preservation, and
  structured dormant rename/claim remediation.
  Prefix API and root `main.rs` inspection functions with `dormant_`; name the
  rename diagnostic function `rename_destination_dormant`.
- [ ] Run red:

  ```bash
  cargo test api::tests::dormant_ -- --nocapture
  cargo test tests::dormant_ -- --nocapture
  cargo test api::tests::rename_destination_dormant -- --nocapture
  ```

- [ ] Add Local GET handlers and Clap `Dormant::{List,Show}`. Reuse
  `encode_path_segment` and make `cli_get` classify non-success status before
  printing.
- [ ] Extend `/api/remove` to map `DormantForgotten` to:

  ```json
  {"forgotten_dormant":"<id>","worktree_preserved":true}
  ```

- [ ] Render the exact safe inspection/unregister commands for dormant
  conflicts. Do not include a TTL or automatic cleanup path.
- [ ] Update the installed skill with `claim`, `dormant list/show`, exact
  unregister semantics, and the rule that rename never claims.
- [ ] Run green:

  ```bash
  cargo test api::tests::dormant_ -- --nocapture
  cargo test tests::dormant_ -- --nocapture
  cargo test api::tests::rename_destination_dormant -- --nocapture
  ```

- [ ] Commit:

  ```bash
  git add src/api.rs src/server.rs src/main.rs skills/ouija/SKILL.md
  git commit -m "feat: inspect and forget dormant identities"
  ```

## Task 10: Close Every External Registration and Ownership Conflict

**Files:**

- Modify: `src/state.rs`
- Modify: `src/hooks.rs`
- Modify: `src/api.rs`
- Modify: `src/daemon_protocol.rs`
- Test: inline protocol/state/API/hook tests

**Consumes:** Combined live/dormant occupancy and resource gates.

**Produces:** One fail-closed conflict policy across generic registration,
managed reservation, backend bind/adopt/rebind, rename, claim, and recovery.

- [ ] Write table-driven red tests for each external entry point against:
  tombstoned destination ID, tombstoned backend pair, live/dormant pane,
  foreign Local/Remote/Human owner, and ID/pane/pair/project lifecycle lease.
- [ ] Include same-owner retries and stale exact callbacks. Assert the complete
  protocol snapshot is unchanged on every rejection. Prefix every table
  function with `dormant_conflict_`.
- [ ] Run red:

  ```bash
  cargo test dormant_conflict -- --nocapture
  ```

- [ ] Route scan registration through `Automatic` naming over both maps.
  Add pure checks to managed reservations and bind/adopt/rebind events so no
  coordinator can bypass dormant pair or ID occupancy.
- [ ] Preserve managed-launch credential and `recover-backend-identity`
  authority rules exactly; do not make tombstones a fallback for incomplete
  legacy repair.
- [ ] Run green:

  ```bash
  cargo test dormant_conflict -- --nocapture
  ```

- [ ] Commit:

  ```bash
  git add src/state.rs src/hooks.rs src/api.rs src/daemon_protocol.rs
  git commit -m "fix: reserve dormant identity resources everywhere"
  ```

## Task 11: Extend the Stateright Model and Invariants

**Files:**

- Modify: `src/daemon_protocol.rs` model module

**Consumes:** New pure events and durable map.

**Produces:** Bounded proofs for continuity and non-overwrite invariants.

- [ ] Replace the current focused `RenameOccupied` counterexample expectation
  with a preservation property once Task 3 is green.
- [ ] Add model actions for eligible/ineligible dormancy, trusted SessionEnd,
  recovery, claim, dormant forget, stale owner callbacks, conflicting
  resources, active/stop boundaries, and persistence-independent pure retries.
- [ ] Add invariants:
  no destination overwrite; at most one distinct owner per complete pair
  across live/dormant/leases; at-most-once tombstone consumption; recovered
  incarnation greater than prior; stale events preserve newer winners; no
  worktree-delete effect from dormancy/recovery/forget; dormant segments always
  closed; accumulated active time never decreases.
- [ ] Run the focused red model before updating the old expectation:

  ```bash
  cargo test daemon_protocol::stateright_model::model_check_occupied_rename_bfs -- --exact --ignored --nocapture
  ```

  Expected before the assertion update: the old “counterexample exists”
  assertion fails because occupied rename is now safe.

- [ ] Update the focused assertion to require no counterexample and run green:

  ```bash
  cargo test daemon_protocol::stateright_model::model_check_occupied_rename_bfs -- --exact --ignored --nocapture
  cargo test model_check_bfs -- --ignored --nocapture
  ```

- [ ] Commit:

  ```bash
  git add src/daemon_protocol.rs
  git commit -m "test: model local identity continuity"
  ```

## Task 12: Add the Isolated Real-Boundary E2E

**Files:**

- Modify: `tests/e2e/lib.sh`
- Modify: `tests/e2e/run-tests.sh`
- Modify: `tests/e2e/Dockerfile` only if the fake backend fixture needs it

**Consumes:** Docker-isolated daemon, tmux server, CLI, HTTP hooks, fake
assistant processes.

**Produces:** One local-suite scenario covering claim, clean dormancy,
replacement-pane recovery, collision-safe rename, inspection, and cleanup.

- [ ] Add a fake Codex (or generic process named `codex`) helper that stays
  alive in a dedicated tmux pane and permits complete backend identity evidence
  without touching host processes.
- [ ] Add a shell test that:
  starts an unregistered fake assistant; invokes
  `TMUX_PANE=<pane> CODEX_THREAD_ID=<opaque> ouija claim chosen-id`; records its
  incarnation; verifies exact retry; parks a complete Claude fixture through
  trusted SessionEnd; verifies `dormant list/show`; resumes the same backend
  pair in a replacement pane; verifies the same public ID and a newer
  incarnation; attempts an occupied rename and verifies both rows; intentionally
  unregisters a dormant row and verifies the worktree still exists.
- [ ] Install a restore `trap` before the first daemon mutation and keep all
  fixture paths inside the Docker test workspace.
- [ ] Run red before wiring the scenario into the local suite:

  ```bash
  tests/e2e/run-e2e.sh local
  ```

  Expected: the new identity-continuity scenario reports its first unmet
  claim/dormancy assertion.

- [ ] Complete fixture wiring without host-daemon assumptions. Do not describe
  Docker as mutating live host state.
- [ ] Run green:

  ```bash
  tests/e2e/run-e2e.sh local
  ```

- [ ] Commit:

  ```bash
  git add tests/e2e/lib.sh tests/e2e/run-tests.sh tests/e2e/Dockerfile
  git commit -m "test: exercise identity continuity across process boundary"
  ```

## Task 13: Final Verification and Diff Review

**Files:** Review every changed file; no planned production changes.

**Consumes:** Completed Tasks 1–12.

**Produces:** Evidence-backed handoff with no uncommitted implementation
changes.

- [ ] Run formatting check. If it fails, return to the task that owns the
  reported file, format there, rerun that task's focused tests, and amend that
  task's commit before continuing:

  ```bash
  cargo fmt --all -- --check
  ```

- [ ] Run all unit/integration tests:

  ```bash
  cargo test
  ```

- [ ] Run the explicit model check:

  ```bash
  cargo test model_check_bfs -- --ignored --nocapture
  ```

- [ ] Run lint:

  ```bash
  cargo clippy --all-targets --all-features
  ```

- [ ] Re-run isolated local E2E:

  ```bash
  tests/e2e/run-e2e.sh local
  ```

- [ ] Review the complete diff and history:

  ```bash
  git status --short
  git diff origin/master...HEAD --check
  git diff --stat origin/master...HEAD
  git log --oneline origin/master..HEAD
  ```

- [ ] Confirm explicitly: no placeholder/stub remains; no absent-source rename
  claim remains; all new events/effects are exhaustively matched; v2 rejects
  unknown versions; transient attestations are not persisted; no remote claim
  route exists; no worktree delete is emitted; every success is after durable
  persistence.
- [ ] Ask parent `ouija` for final review and next action. Do not push:

  ```bash
  printf '%s\n' 'done: identity continuity implementation is committed locally; all focused, full, model, clippy, and isolated local E2E checks pass. What should I do next?' \
    | ouija ask ouija --stdin --from rootid-fix
  ```
