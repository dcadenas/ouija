# Session-Owned Context Rollover Implementation Plan

> **For Codex workers:** implement only the assigned slice, preserve earlier
> commits, run the focused tests, and make at most one local commit. Do not
> push, install globally, restart live sessions, or create schedules.

**Goal:** Make intentional context rollover reliable while keeping semantic
handoff and enrollment policy outside Ouija daemon core.

**Architecture:** Add deterministic restart prompt composition, a local
session-owned continuation helper, and a non-lifecycle scheduler injection
mode. Use existing lifecycle ownership/incarnation checks; expose incarnation
and parent only for verification and operator visibility.

**Tech stack:** Rust, Axum, Clap, Tokio, Serde, git CLI, existing Ouija
scheduler and backend adapters.

---

## Slice 1: Deterministic restart inputs

**Files:** `src/main.rs`, `src/api.rs`, `src/nostr_transport.rs`,
`skills/ouija/SKILL.md`, `README.md`

1. Add failing table tests for base prompt replacement, stored fallback,
   suppression, one-shot append, and non-persistence.
2. Add CLI parser/file-input tests for `--backend`,
   `--suppress-stored-prompt`, and `--one-shot-file`.
3. Add API request fields and validate the explicit backend using the existing
   backend registry.
4. Implement one pure prompt-resolution function and pass its result through
   OpenCode soft restart and hard fallback without recomputation.
5. Persist only an explicit base replacement; never persist launch-only text.
6. Update shared CLI guidance and run focused tests, then `cargo test`.

## Slice 2: Local continuation lifecycle

**Files:** new `src/rollover.rs`, `src/main.rs`, `src/api.rs`,
`skills/ouija/SKILL.md`, backend skill-packaging assertions, `Cargo.toml` and
`Cargo.lock` only if locking cannot use an existing dependency

1. Add failing tests for schema bounds, storage path/permissions, state
   fingerprinting, pending/adopted transitions, wrong token/session/
   incarnation, expiry, path/HEAD/branch/dirty mismatch, and idempotent retry.
2. Implement `rollover prepare --stdin`, deriving exact caller identity and
   repository binding from live state.
3. Write pending records under the Ouija user data directory with a lock and
   atomic replacement; reject repository-local storage.
4. Implement `rollover adopt TOKEN`, requiring a newer exact incarnation and
   unchanged live binding before atomically adopting and printing payload.
5. Expose `session_incarnation` as a decimal string in relevant status output
   without changing metadata freshness.
6. Document safe-boundary creation, launch-only adoption, mismatch refusal,
   cleanup, and the authority boundary. Verify all backend packages use the
   same skill.
7. Run focused tests, `cargo test`, and `cargo clippy --all-targets
   --all-features`.

## Slice 3: Exact-target audit injection

**Files:** `src/scheduler.rs`, `src/api.rs`, `src/main.rs`, `src/admin.rs`,
`skills/ouija/SKILL.md`, `README.md`, `tests/e2e/`

1. Add failing unit tests for `OnFire::InjectOnly`: live local delivery and
   closed failure for missing, paneless, dead, or remote targets, with no lease,
   restart, worktree, or session creation.
2. Add `--inject-only` CLI/API validation requiring an explicit target and
   render the mode in task listings/admin output.
3. Implement the mode through the exact current owner and existing message
   injection path only.
4. Expose `parent_session` in status/list output for policy-side explicit child
   selection.
5. Document one root schedule with an exact child allowlist, non-recursive
   ordinary child nudges, null-parent handling, and the prohibition on
   native-subagent discovery or inferred enrollment.
6. Add a local e2e case with a restore/cleanup trap and run it, then run
   `cargo test` and Clippy.

## Integration review

1. Review each slice against the design before accepting its commit.
2. Run `cargo fmt --check`, `cargo test`, and
   `cargo clippy --all-targets --all-features` on the combined branch.
3. Inspect the final diff for persisted one-shot content, semantic policy in
   daemon core, lifecycle actions in `InjectOnly`, repository-local artifacts,
   and accidental native-subagent discovery.
4. Report commits and verification to `hub-cx`; do not push or mutate live
   sessions/tasks.
