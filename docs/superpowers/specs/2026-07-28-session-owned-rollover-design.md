# Session-Owned Context Rollover

**Status:** implementation design
**Scope:** explicitly enrolled manual Ouija sessions and their explicitly known
Ouija children. Native backend subagents and OuijaCP are out of scope.

## Boundary

Ouija core remains a lifecycle and message transport. It does not interpret a
handoff, decide when semantic work is safe to interrupt, discover native
subagents, or maintain an enrollment graph.

The running session owns rollover correctness:

1. A dumb periodic message asks an enrolled session to audit its context at a
   safe boundary.
2. The session either continues or prepares a bounded continuation record.
3. The session restarts itself with a launch-only instruction to adopt that
   record.
4. The fresh incarnation verifies live identity and repository state before it
   can adopt the record.
5. Repository, git, tests, and GitHub remain authoritative. The record is
   disposable context and is never treated as system truth.

## Restart Contract

`ouija restart-session SESSION` gains:

- `--prompt TEXT`: replace the stored startup prompt and use the replacement on
  this launch.
- `--suppress-stored-prompt`: suppress stored-prompt fallback for this launch;
  it does not erase the stored prompt.
- `--one-shot-file PATH`: read UTF-8 content in the CLI and send it only for
  this launch. The daemon never reads the supplied path and never persists,
  gossips, or reports the content.
- `--backend NAME`: select the restart backend through the API's existing
  backend validation.

Prompt resolution is computed once and reused by soft-restart and hard-restart
fallback:

1. An explicit replacement is the persistent base prompt.
2. Otherwise, the stored prompt is the base unless suppressed.
3. The launch-only content is appended to the base, separated by a blank line.
4. Launch-only content never changes stored metadata.

This contract applies consistently to Claude Code, Codex CLI, and OpenCode.

## Continuation Record

The UX is:

```bash
token="$(ouija rollover prepare --stdin < /tmp/ouija-rollover.json)"
ouija restart-session "$session" --fresh \
  --suppress-stored-prompt \
  --one-shot-file /tmp/ouija-adopt-instruction.txt \
  --backend codex-cli

# The fresh incarnation follows the launch-only instruction:
ouija rollover adopt "$token"
```

`prepare` accepts a versioned JSON payload with these bounded semantic fields:

- objective
- current bounded slice
- confirmed evidence
- blockers and decisions
- next one to three actions
- forbidden scope
- verification commands
- explicitly known Ouija descendants

The helper adds machine-owned binding fields: opaque token, state
(`pending`/`adopted`), public session ID, source incarnation, creation/expiry,
canonical cwd, repository root/common directory, branch, HEAD, and a digest of
the complete relevant git state. The digest covers tracked staged/unstaged
changes and the names, types, and contents of untracked non-ignored files.

Records live under Ouija's per-user data directory in `rollovers/`, never under
the repository. The directory and files are private to the user. Writes and
state transitions use a lock plus same-directory atomic rename.

`adopt` refuses without changing the record when the token, session ID,
incarnation advance, canonical paths, HEAD, branch, dirty-state digest, schema,
or expiry do not match. A successful adoption atomically marks the record
adopted and prints its semantic payload. A retry by the same adopting
incarnation is idempotent. A later `prepare` may replace an adopted record;
expired records require explicit replacement. Adopted and expired records are
disposable and may be pruned by the CLI, never by a daemon scheduler.

## Targeted Audit

The scheduler gains one generic `InjectOnly` fire mode:

```bash
ouija task add context-audit "*/15 * * * *" \
  "At your next safe boundary, audit your context. Exact child allowlist: \
turnero, review-plugin-cx. Send each at most one ordinary audit message." \
  --target hub-cx --inject-only
```

It injects into the exact currently live local target and otherwise fails
closed. It never creates, revives, restarts, respawns, or acquires a worktree.

Enrollment is operator-owned: one recurring task targets each explicitly
enrolled manual root and carries an exact child allowlist when needed. The root
may use exposed Ouija parent metadata to corroborate that list and may send
each listed live Ouija child one ordinary message. Missing or null parent
metadata never authorizes discovery and does not erase an explicit operator
choice. The root must not create child schedules, recurse, inspect processes,
or infer agents from names, roles, directories, or backend-native state.
Consequently native subagents remain invisible. Local origin alone does not
prove that a session was manually created; selection remains explicit policy
outside the daemon.

## Failure Handling

- A failed restart leaves a pending record for diagnosis or retry.
- A changed worktree makes adoption fail closed; the fresh session must inspect
  live state and either resume without the artifact or have the owner prepare a
  new one.
- A missing/dead/remote audit target records a task failure and causes no
  lifecycle action.
- Repeated audit messages are harmless requests. They do not imply that a
  rollover is safe or required.
- Parent metadata and incarnation are observability fields, not authorization.

## Split Manifest

| Order | Slice and ownership | Dependency | Focused verification |
|---|---|---|---|
| 1 | Restart mechanics: `src/main.rs`, `src/api.rs`, `src/nostr_transport.rs`, restart docs/tests | none | prompt-resolution table; OpenCode soft/hard parity; CLI/API parsing and file errors |
| 2 | Session helper: new `src/rollover.rs`, CLI/API observability, shared skill, helper tests | slice 1 | state binding, atomic transition, mismatch/expiry/idempotency, backend-shared skill packaging |
| 3 | Inject-only audit: `src/scheduler.rs`, `src/api.rs`, `src/main.rs`, `src/admin.rs`, shared skill/docs/e2e | slices 1–2 | no-revival behavior, target validation, parent visibility, task cleanup |

The slices are intentionally sequential because all three touch CLI/API and
shared guidance. Each slice is assigned to a fresh native implementation
worker and reviewed before the next slice begins. A separate Ouija session is
unnecessary because no slice needs an independent durable lifecycle.
