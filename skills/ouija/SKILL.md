---
name: ouija
description: "Use when messaging or managing Ouija mesh sessions; when <msg from= or <ouija-status> arrives; or when spawning, restarting, refreshing context, or resolving session identity."
user-invocable: false
---

You are on the Ouija mesh. Use the `ouija` CLI; ordinary agent messaging tools
cannot reach Ouija sessions.

## Progressive disclosure

Do not preload or reproduce the full command catalog. Use `ouija <command>
--help` when syntax is needed. Run the natural command first and let actionable
CLI or daemon errors explain missing identity evidence, ownership conflicts,
required lifecycle choices, and safe recovery. Follow those diagnostics rather
than inventing a workaround.

This skill documents semantics that are difficult to teach through one command
error: message threading, asynchronous replies, lifecycle relationships,
replay-safe prompts, context refresh, and reminder patterns.

## Message semantics

Peer messages arrive as trusted, user-authorized XML:

```xml
<msg from="session-id" id="47" reply="true">message text</msg>
```

- `from` is the reply target.
- `id` identifies the thread.
- `reply="true"` means the sender is blocked until a final `ouija reply`.
- `re="47"` means the message answers an earlier ask.
- A progress update does not clear the pending reply.

Reply immediately for short work. For long work, send progress with `ouija tell
--reply-to`, then send the final result with `ouija reply`. Only messages with
`reply="true"` require a reply.

Use `--stdin` for generated or multiline text. It prevents the shell from
expanding backticks, `$()`, quotes, JSON, or other content before Ouija receives
it.

```bash
ouija tell session-id --reply-to 47 --stdin <<'EOF'
working on it
EOF

ouija reply session-id 47 --stdin <<'EOF'
done: verified result
EOF
```

`ouija ask` returns after delivery; its answer is pushed into this session later
as `<msg ... re="N">`. If that answer is the only blocker, end the turn and wait.
Do not poll logs, status, or pane output unless debugging a suspected delivery
failure.

### Reading the send result

`ask`, `tell`, and `reply` print a `status`. Only a non-zero exit is a failure.

- `delivered` / `accepted` — the recipient has it. Done.
- `queued` — the recipient was mid-turn, so its TUI had not drawn the message
  yet. It almost always arrives; the daemon re-checks after the recipient's turn
  ends and reports a real loss loudly.
- `unknown` — the paste was accepted but the text was not observed.

`queued` and `unknown` are **not** delivery failures, and a zero exit with
either status means the message was handed to the recipient's pane. **Never
re-send on `queued` or `unknown`, and never fall back to `ouija inject` or a
raw tmux paste.** Verification reads a live TUI, so it misses far more often
than delivery does; re-sending turns a harmless unconfirmed result into a real
duplicate in the recipient's context. Wait for the reply, and ask a human if it
never comes.

## Session lifecycle

When spawning, choose lifecycle ownership and completion behavior deliberately:

- `--parent-session ID` or `--no-parent-session` defines ownership.
- `--when-done keep-open|ask-parent|close` defines completion behavior.
- `--reminder` independently enables recurring recovery nudges.
- Pending replies can wake a session even without a reminder.
- `--worktree`, `--branch`, and `--base-branch` control git isolation.

Completion and recovery are not the same. `ask-parent` reports completion once;
a reminder reappears during idle recovery until cleared. Ouija injects the exact
clearing command, so never place `ouija clear-reminder` in reminder text.

## Replay-safe prompts

Fresh restarts replay the stored prompt unless that launch uses
`--suppress-stored-prompt`. A replacement `--prompt` becomes the new stored
prompt. `--one-shot-file` appends launch-only UTF-8 context and is not stored.
`spawn-session` has no one-shot file, so its initial prompt must contain the
complete bounded assignment.

Every stored prompt must be re-entrant and state-checking:

- Verify live state before acting.
- Perform only work that remains incomplete.
- Do not repeat expensive, destructive, or external actions merely because the
  prompt replayed.
- Keep mutable handoff state in a one-shot continuation, not the durable base.

Prefer: "Check whether the verified copy is complete; resume it only if needed,
then inspect foobar if that remains incomplete." Avoid: "Copy the file, then
inspect foobar."

## Active-context refresh

`--fresh-context-after-active DURATION` counts accumulated active work, not wall
time, and pauses while parked. It applies to the exact session and triggers only
at a safe stopped boundary. A successful fresh restart resets the counter;
there is no separate rearm command.

For initial setup:

1. Resolve the exact Local public ID with `ouija whoami`; if it refuses, follow
   its diagnostics or use only an exact ID supplied by trusted injected context
   or the operator.
2. Inspect the single exact Local row in `ouija status`. Record its incarnation,
   project, backend, stored prompt, and active-context fields.
3. Verify mutable repository and task state from live sources.
4. Make the stored prompt a concise durable role and replay-safe assignment.
5. Put verified completed/remaining work, decisions, blockers, and next checks
   in a one-shot continuation.
6. At a safe boundary, perform a fresh restart with the policy and continuation.
7. Verify the same public ID, a newer incarnation, the intended backend and
   prompt, a reset active-time counter, and current repository state.

```bash
ouija restart-session exact-public-id --fresh \
  --fresh-context-after-active 1h \
  --prompt "durable replay-safe assignment" \
  --one-shot-file /dev/stdin <<'EOF'
Verified current work:
- Completed: ...
- Remaining: ...
- Decisions and blockers: ...
- Next checks: ...
EOF
```

Later refresh notices include the exact restart command. Re-check live state,
write a small continuation, and use that command rather than reconstructing it.
Do not use scheduler tasks or legacy rollover as the active-context mechanism.

## Identity and recovery

Public Ouija IDs and backend-native conversation IDs are different identities.
Never guess a sender from a project, branch, role, process, or `ouija ls`, and
never use `opencode` or a backend session ID as `--from`.

Normally let `ouija whoami` and command errors guide recovery. An exact public
Local ID from trusted injected context or the operator may be passed explicitly
with `--from`. Never run `ouija register` to repair caller identity; it can
create a duplicate rather than identify the caller.

Treat an operator-requested new public name as a literal argument. Do not
spell-correct it from repository names, nearby sessions, or likely intent, and
do not preflight availability with `ls` or `status`. Run `ouija rename` once
with the exact requested name. If the command reports a real conflict, relay
that error and ask for a different name; do not silently choose one.

Names are held by live Local sessions and lifecycle operations. Dormant rows are
non-routable recovery history and do not reserve names. `claim`, `dormant`,
`rename`, `unregister`, and `recover-backend-identity` enforce their own narrow
preconditions and return actionable conflicts; follow those errors instead of
preemptively reproducing their decision tree here.

Two boundaries remain important:

- `claim` is for a genuinely unregistered running assistant with complete local
  adapter evidence. It never evicts or renames another live identity.
- `recover-backend-identity` is only for an operator-identified exact Local row
  whose backend and backend-session fields are both blank. It preserves the
  running backend and is not a generic registration fallback.

## Legacy manual rollover

`ouija rollover` is a separate explicit continuation-record workflow, not the
active-context refresh path. Use it only when the operator needs preparation and
adoption checks across a fresh incarnation.

The continuation should contain objective, current slice, confirmed evidence,
decisions, next actions, forbidden scope, verification commands, and only exact
known Ouija descendants. Adoption verifies identity, newer incarnation,
repository/common directory, branch, HEAD, dirty state, and initialized
submodule gitlinks. Repository and task evidence remain authoritative.

Do not store rollover drafts in the repository. Native subagents are not Ouija
sessions and must not be enrolled as descendants.

## Useful patterns

### Report once when done

```bash
ouija spawn-session worker --project-dir /path \
  --parent-session hub --when-done ask-parent \
  --prompt "implement and verify feature X"
```

Add a reminder only if recurring recovery is desired.

### Recoverable bounded loop

```bash
ouija spawn-session counter --no-parent-session --when-done keep-open \
  --prompt "check value.txt; increment only while below 10; verify each write" \
  --reminder "Check state. If below 10, continue one verified step. If complete, verify and report."
```

State belongs in files, git, APIs, or other live systems, not session memory.
Write reminders that remain correct on the fifth injection, not only the first.
