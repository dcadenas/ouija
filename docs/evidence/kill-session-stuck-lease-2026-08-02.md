# Stuck Stopping lease: `turnero-issue-50` (2026-08-02)

Live evidence captured 2026-08-03, while the wedged state was still present and
before any daemon restart cleared it. Ouija revision `1af9de5` (v0.1.0-alpha.235).

A failed `ouija kill-session` left a durable `Stopping` lease that no retry could
clear, because `claim_existing_stop` rejects while any lease exists for the
public session ID. Reported by the `turnero` coordinator, diagnosed by
`ouija-org`, re-verified here against source and live state.

## Why this file exists

The daemon log contains **no record of the failure**. The success path logs
`aborted opencode server session`; both abort-failure branches return a
`KillSessionResult::failed(...)` string to the CLI caller and log nothing. Ten
successful aborts appear in the log that day; `turnero-issue-50` is absent. So
without this capture the trigger is unreconstructable — the only server-side
artifact is the lease itself, which a restart destroys.

That observability gap is itself worth fixing.

## Durable evidence pointer

The backend holds the transcript independently of the daemon and survives daemon
restarts, so this pointer stays resolvable after the wedge is cleared:

- backend session: `ses_03b8ad41cffeHolUU7mLGVXvuK` (opencode, port 8200)
- project dir: `/home/daniel/.ouija/worktrees/turnero/turnero-issue-50`
- retrieve: `GET http://127.0.0.1:8200/session/ses_03b8ad41cffeHolUU7mLGVXvuK/message`
  with header `x-opencode-directory: <project dir>` (98 messages at capture time)

## The stuck lease

Sole lease in `sessions.json` at capture:

```json
{
  "turnero-issue-50": {
    "owner": { "session_id": "turnero-issue-50", "incarnation": 1784923380514537260 },
    "phase": "stopping",
    "backend": "opencode",
    "backend_session_id": "ses_03b8ad41cffeHolUU7mLGVXvuK",
    "backend_session_owner": { "session_id": "turnero-issue-50", "incarnation": 1784923380514537260 },
    "project_dir": "/home/daniel/.ouija/worktrees/turnero/turnero-issue-50",
    "project_dir_owner": { "session_id": "turnero-issue-50", "incarnation": 1784923380514537260 },
    "project_dir_cleanup_on_abandon": true,
    "inert_pane": "%67",
    "inert_pane_owner": { "session_id": "turnero-issue-50", "incarnation": 1784923380514537260 }
  }
}
```

Reproduced verbatim at capture time, matching every retry the operator saw:

```
$ ouija kill-session turnero-issue-50
{"outcome":"superseded","result":"session 'turnero-issue-50' backend exit was superseded (Rejected)"}
```

Original failure as reported by the operator:

```
{"outcome":"failed","result":"opencode abort for session ses_03b8ad41cffeHolUU7mLGVXvuK failed:
 error sending request for url (http://127.0.0.1:8200/session/.../abort);
 stop authority retained for recovery"}
```

## Close-out sequence (the trigger)

From the backend transcript, UTC:

| time | event |
|---|---|
| 22:30:33 | `<msg from="turnero" id="1785684571">` — merge/close-out tell delivered |
| 22:30:45–22:30:49 | assistant acknowledges, turn **completed** |
| 22:33:26 | `<msg from="turnero" id="1785684573" re="1785684570" done="true">` — final tell |
| 22:33:31 | assistant replies `Session complete.`, turn **completed** |
| 22:33:31.509 | last daemon-log line for this session |
| ~22:33:34 (inferred) | `kill-session`, ~8s after the tell per operator report |

**This corrects the reported trigger.** The operator's hypothesis was that the
kill landed while the backend was still mid-turn. The transcript shows the final
turn *completed* at 22:33:31, five seconds after the tell. A kill ~8s after the
tell therefore arrived at an idle session. The exact kill timestamp is not
recorded (see observability gap above), so this is an inference from the
operator's stated timing, not a measurement — but "killed mid-turn" is not
supported by the transcript.

Also ruled out: `opencode serve` was not down. PID 108227 has run continuously
since 2026-08-01 14:27, so the abort went to a live server.

## Why the abort failed, and what it means for the fix

The abort POST carries a **5-second timeout** (`nostr_transport.rs`, the
`with_owned_backend_cleanup` block). A `reqwest` timeout surfaces as exactly the
observed `error sending request for url ...` text, as does a refused connection.
The reported string alone does not distinguish them.

That distinction is the crux:

- **connect refused** — the abort never arrived. Releasing the lease is safe and
  retry would work.
- **timeout** — the abort may have been received and applied. Releasing the lease
  is *not* obviously safe.

The codebase already draws exactly this line, one function away, for message
delivery — `classify_prompt_async_fallback` maps `error.is_connect()` to
`DefiniteNonAcceptance` and everything else, timeouts included, to `Ambiguous`.
The kill path does not use it: both error branches collapse into a single
"retain stop authority" outcome.

So the narrow fix is to apply the existing classification to the abort. Note the
honest consequence: if this instance was a **timeout**, it classifies as
`Ambiguous` and would *still* retain the lease. The narrow fix reduces how often
this happens; it does not rescue this case. Only a scoped recovery path that does
not require a daemon restart addresses the operator-visible harm, which is that
clearing one stuck session means disrupting every unrelated session on the host.

## Worktree disposition

`project_dir_cleanup_on_abandon: true`, so the daemon restart that clears this
lease will also delete the worktree. Verified safe before capture: the worktree
was clean, and its HEAD `adef6410` is contained in `main`, `origin/main`, and
merge commit `9d43cba4` (dcadenas/turnero#50, merged and closed). Nothing is lost
by that deletion.

## Verified source references (revision `1af9de5`)

- `src/daemon_protocol.rs:2187` — `claim_existing_stop`; lines 2193-2195 reject
  when any lease exists for the public session ID, regardless of phase.
- `src/nostr_transport.rs:1704-1712` — maps `Rejected` to the `superseded` result.
- `src/nostr_transport.rs:1786-1793` — abort POST with the 5s timeout.
- `src/nostr_transport.rs:1798` — the adjacent no-response branch **does** call
  `abort_lifecycle`, so the release mechanism exists and is already used.
- `src/nostr_transport.rs:1809-1821` — both abort-error branches return failure
  without releasing, and without logging.
- `src/nostr_transport.rs:6936-6954` — `classify_prompt_async_fallback`, the
  existing connect-vs-ambiguous precedent.
- `src/main.rs:1710` — `restore_persisted_sessions`; replays the abandoned abort
  at daemon start before releasing pane, worktree, row and public ID.
- `src/main.rs:5802`, `src/main.rs:5960` — tests encoding both restore outcomes.
