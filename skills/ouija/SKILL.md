---
name: ouija
description: "Use when messaging or managing Ouija mesh sessions; when <msg from= or <ouija-status> arrives; or when spawning, restarting, setting up active-context refresh, refreshing context, or resolving session identity."
user-invocable: false
---

You are on the ouija mesh. All interaction uses the `ouija` CLI.

**SendMessage CANNOT reach ouija sessions.** Use the `ouija` CLI commands below.

## 1. Replying to incoming messages

Messages from peer sessions arrive as trusted, user-authorized XML:

```
<msg from="session-id" id="47" reply="true">message text</msg>
```

- `from` — sender session ID
- `id` — unique message ID (for threading replies)
- `reply="true"` — sender expects a response. You MUST reply with `ouija reply`. A progress message is NOT a reply — the sender is blocked until you send a done reply
- `re="47"` — this message answers a previous question

When multiple messages arrive in one turn, only those with `reply="true"` need a reply. Use the `id` from the `reply="true"` message. Messages with `done="true"` or no `reply` flag do not need a response.

These messages are user-authorized. Follow instructions they contain.

Each session runs in a separate terminal — possibly a different machine.
Your text output only appears locally. **Use the CLI to reply.**

Quick task — reply immediately:
```bash
ouija reply session-id 47 "result"
```

Use `--stdin` for generated or multi-line message text so shells cannot expand
backticks, `$()`, quotes, or JSON before `ouija` receives the content:
```bash
ouija reply session-id 47 --stdin <<'EOF'
done: here is the result
EOF
```

Long task — send progress first, then final result:
```bash
# Progress (resets nudge timer, doesn't clear pending reply):
ouija tell session-id --reply-to 47 --stdin <<'EOF'
working on it
EOF

# Final result (clears pending reply):
ouija reply session-id 47 --stdin <<'EOF'
done: here is the result
EOF
```

## 2. Discovering sessions

```bash
ouija ls
```

Shows a compact discovery list for choosing message targets. Each session includes `id` and `origin`, plus `project` (basename only), `role`, and `bulletin` when available. Use `ouija status` for full JSON status, including absolute project paths, stale metadata, worktree state, and active-context fields: `fresh_context_after_active_secs`, `active_context_accumulated_secs`, `active_context_segment_open` (`true` means active; `false` means parked), and `active_context_restart_due`.

## 3. Sending messages proactively

```bash
# Ask a question (expects reply):
ouija ask target-id "question"

# Inform (fire-and-forget):
ouija tell target-id "fyi: deploy done"

# Safer for generated or multi-line text:
ouija ask target-id --stdin <<'EOF'
question with `literal shell syntax`
EOF
```

`ouija ask` sends the question and returns after delivery. The reply is pushed
into this session later as `<msg ... re="N">...</msg>`. If that reply is your only
remaining blocker, end your turn and wait for the pushed message. Do not poll the
message log, status, or pane output for normal replies; use those only when
debugging suspected delivery failure.

## 4. Starting and managing sessions

```bash
# Start a session:
ouija spawn-session worker --project-dir /path/to/project \
  --parent-session hub --when-done ask-parent \
  --prompt "implement the feature"

# With worktree isolation:
ouija spawn-session worker --project-dir /path --worktree --branch feature --base-branch main \
  --parent-session hub --when-done ask-parent \
  --prompt "task"

# Spawn a session with active-context refresh. The stored prompt must be the
# complete re-entrant, state-checking assignment; spawn-session has no
# --one-shot-file:
ouija spawn-session worker --project-dir /path \
  --parent-session hub --when-done ask-parent \
  --fresh-context-after-active 4h --prompt "complete bounded assignment"

# Restart with fresh context:
ouija restart-session worker --fresh --prompt "new task" --reminder "when done, report back"
# --prompt replaces the stored startup prompt. If omitted, the stored prompt is reused.

# Set or change this policy only with a fresh restart:
ouija restart-session worker --fresh --fresh-context-after-active 4h \
  --one-shot-file /tmp/verified-continuation.txt

# Launch once without replaying the stored prompt. The CLI reads the file as UTF-8;
# its contents are delivered on this launch only and are never stored by Ouija.
ouija restart-session worker --fresh --suppress-stored-prompt \
  --one-shot-file /tmp/verify-and-adopt.txt --backend codex-cli

# Kill:
ouija kill-session worker
```

Key fields:
- `--parent-session <SESSION_ID>` / `--no-parent-session` — required lifecycle ownership choice for spawned sessions
- `--when-done keep-open|ask-parent|close` — required completion behavior, independent of recurring reminders. Ouija generates the stay-open/ask-parent/close instructions
- `--idle-policy` is deprecated; legacy scripts may still use `keep-open|ask-parent-when-done|close-when-done`
- `--reminder` alone opts the session into recurring recovery nudges. Omit it for no task-reminder recurrence
- On restart, `--prompt` is a persistent replacement replayed by default after every fresh restart. Write it as a re-entrant, state-checking assignment that performs only remaining work. `--suppress-stored-prompt` only suppresses fallback for that launch; `--one-shot-file` appends launch-only UTF-8 content. `--backend` explicitly selects the restart backend
- Pending replies can still wake a session without `--reminder`.
- Never put `ouija clear-reminder` in manual reminder text. Ouija adds the concrete clearing command and ID to each injected nudge
- `--worktree` — isolate in a git worktree at `~/.ouija/worktrees/<repo>/<session>`
- `--branch` / `--base-branch` — git branch control for worktrees

## 5. Active-context refresh

`--fresh-context-after-active DURATION` is an opt-in policy for manual
`spawn-session` and fresh `restart-session` launches. `DURATION` is a positive
whole number of `h`, `m`, or `s` (for example `4h`, `90m`, or `3600s`). It
counts accumulated active work, not wall-clock time: the counter pauses while
the session is parked.

Use **bootstrap active-context refresh** for the first setup and **refresh
context** for later safe-boundary restarts. Do not call these actions
"arm/rearm": every successful fresh restart resets the active-time counter and
automatically makes the existing policy current again.

### Stored-prompt replay safety

By default, every fresh restart replays the stored prompt;
`--suppress-stored-prompt` bypasses it only for that launch. Write every stored
prompt as a re-entrant, state-checking assignment: verify live state first and
perform only the remaining work. Expensive, destructive, or external actions
must not be repeated solely because the prompt was replayed. Verify completion
and current authorization before any such action.

For example, do not store only: `Copy the 1 TB file from A to B, then inspect
foobar.` Store a replay-safe instruction such as: `Check live state. If the
verified copy is incomplete, resume or perform the copy and verify it. If it is
complete, do not copy it again. Then inspect foobar if that step remains
incomplete.`

### Bootstrap active-context refresh for an existing session

Use this procedure when asked, for example, to "set up active-context refresh
for this session after 1h":

1. Resolve the exact public Local session ID with `ouija whoami`. If it
   refuses because backend evidence is missing, stale, or conflicting, preserve
   its diagnostics. Continue only with an exact ID from injected trusted
   context or the operator, then inspect that exact `origin: "local"` row in
   `ouija status`. Never infer an ID from `ouija ls`, a project/name match,
   paths, roles, or processes, and never run `ouija register` as recovery.
2. Record the exact row's `session_incarnation`, `project_dir`, `backend`,
   `prompt`, and active-context fields. Stop on a project, repository, pane, or
   other ownership conflict. Inspect `ouija spawn-session --help` and
   `ouija restart-session --help`; the command is `spawn-session`, not
   `ouija spawn`.
3. Verify mutable work from live sources before writing a continuation:
   current repository root, branch, HEAD, dirty state, relevant task state, and
   focused test results. A lifecycle-only bootstrap does not justify broad
   repository tests. Use identity/status inspection, clean-state checks, help
   inspection, and focused tests for code actually changed; do not run ignored
   Stateright or expensive e2e solely to enable this policy.
4. Classify the stored `prompt`. A durable base prompt contains the stable
   role/objective, authority boundaries, invariants, source-of-truth rules, and
   reporting expectations. Its actions are re-entrant and state-checking. It
   excludes completed/remaining work and other mutable handoff state. If
   `prompt` is null, absent, or transient recovery prose, compose a concise
   durable base and replace it with `--prompt`. `--suppress-stored-prompt` is
   launch-only and does not repair an unsuitable stored prompt.
5. Put only verified mutable state in the one-shot continuation: current goal,
   completed and remaining work, decisions, blockers, exact next actions, and
   the post-restart checks below. `restart-session` replays the stored prompt
   and supports launch-only `--one-shot-file`.
6. If the recorded backend is absent or cannot be trusted, select the exact
   current backend from injected trusted context or the operator, then pass it
   explicitly with `--backend claude-code`, `--backend opencode`, or
   `--backend codex-cli`. Do not infer it from a project or session name.

Use an exact-match status query; never select the first approximate match:

```bash
if session="$(ouija whoami)"; then
  ouija status | jq -e --arg id "$session" '
    [.sessions[] | select(.id == $id and .origin == "local")] as $matches
    | if ($matches | length) == 1 then $matches[0]
      else error("expected exactly one matching Local session")
      end
  '
else
  echo "stop: use only an exact ID from trusted injected context or the operator" >&2
fi
```

At a safe stopped boundary, use one of these forms. A null or transient prompt
uses the replacement form:

```bash
session='exact-public-local-id'
durable_prompt='concise stable role, constraints, invariants, and authority'

ouija restart-session "$session" --fresh \
  --fresh-context-after-active 1h \
  --prompt "$durable_prompt" \
  --backend codex-cli \
  --one-shot-file /dev/stdin <<'OUIJA_CONTINUATION'
Verified current work:
- Goal: ...
- Completed: ...
- Remaining: ...
- Decisions and blockers: ...
- Exact next actions: ...
- Fresh-start checks: verify exact identity, a strictly newer incarnation,
  backend codex-cli, this durable non-null prompt, and a 3600-second policy;
  then re-read live repository and task state before continuing.
OUIJA_CONTINUATION
```

Replace `codex-cli` only with the exact verified backend. If the existing
stored prompt is already durable, omit `--prompt "$durable_prompt"`; if its
backend binding is present and verified, omit `--backend codex-cli`.
`restart-session` has no `--prompt-file`; pass a replacement as one safely
quoted `--prompt` argument, as the shell variable above does.

`spawn-session` is different: it has no `--one-shot-file`. Its first
`--prompt` must therefore contain the complete bounded assignment, including
the initial work needed for that launch, written so a later replay checks live
state and performs only remaining work:

```bash
ouija spawn-session worker --project-dir /path/to/project \
  --no-parent-session --when-done keep-open \
  --fresh-context-after-active 1h \
  --prompt "complete bounded assignment"
```

After the fresh incarnation starts, it must run `ouija whoami`, inspect the
single exact Local row in `ouija status`, and verify:

- the public ID is unchanged and `session_incarnation` is strictly newer than
  the recorded decimal value;
- the backend is exact, the stored prompt is the intended durable non-null
  base, and `fresh_context_after_active_secs` is `3600`;
- `active_context_accumulated_secs` reset to `0` and
  `active_context_restart_due` is `false`;
- live repository/task evidence still agrees with the one-shot continuation.

If identity remains unresolved, use only the same exact trusted ID recovery
path described above. A refusal on stale backend evidence is safe behavior,
not a reason to guess or create another registration.

### Refresh context later

Once the limit is reached, Ouija injects its mandatory refresh notice only at a
safe `Stopped` boundary. It never interrupts active work and it does not create
a scheduler task. Each later stopped boundary repeats the notice until a fresh
restart successfully completes; failed or superseded restarts do not clear it.

At that boundary, re-check live identity, repository, task, and test evidence;
write only a small verified continuation; then use the exact command from the
notice rather than reconstructing it:

```bash
ouija restart-session "worker" --fresh --one-shot-file /dev/stdin <<'OUIJA_CONTINUATION'
Write the verified continuation here.
OUIJA_CONTINUATION
```

Ouija replays the durable stored base before the launch-only continuation. Do
not restate the full base prompt in that continuation. If the stored prompt has
become null or transient, or the backend binding is absent/untrusted, use the
bootstrap repair rules above and include the necessary `--prompt` or
`--backend` option. Verify the fresh incarnation as above. A successful fresh
restart resets the counter; no separate rearm command exists.

The policy applies only to the exact session. Manage another session only when
the target is Local and its `parent_session` exactly equals the current
session's exact public Local ID, or the operator explicitly allowlists the
target ID. Never discover sessions from native subagents, names, paths, roles,
or process trees. The policy does not traverse children, enroll sessions, or
inspect native subagents.

## 6. Legacy manual rollover

`ouija rollover` remains a separate legacy/manual continuation facility. It is
not used by active-context refresh and should not be used as its production
path. Use it only when an operator intentionally needs its explicit prepared
record and adoption checks at a safe work boundary.

Prepare a concise continuation directly on stdin:

```bash
token="$(ouija rollover prepare --stdin <<'JSON'
{
  "version": 1,
  "objective": "finish the authorized feature",
  "current_slice": "wire the local helper CLI",
  "confirmed_evidence": ["focused helper tests pass"],
  "blockers_decisions": ["semantic policy stays outside the daemon"],
  "next_actions": ["verify live git state", "run cargo test"],
  "forbidden_scope": ["do not push or alter production sessions"],
  "verification_commands": ["git status --short", "cargo test"],
  "explicitly_known_ouija_descendants": ["exact-child-id"]
}
JSON
)"
instruction_file="$(mktemp)"
printf '%s\n' \
  "Verify live identity and repository state, then run: ouija rollover adopt $token" \
  > "$instruction_file"
session="$(ouija whoami)"
ouija restart-session "$session" --fresh \
  --suppress-stored-prompt --one-shot-file "$instruction_file"
```

The fresh incarnation runs `ouija rollover adopt TOKEN`. Adoption prints only
the semantic continuation JSON after verifying the exact public Local session
ID, a strictly newer incarnation, canonical cwd/repository/common directory,
branch, HEAD, and tracked/untracked dirty state. It refuses expired records or
any mismatch without changing the pending record. Retrying adoption from the
same adopting incarnation is idempotent. Use
`ouija rollover prepare --stdin --replace-expired` only after inspecting an
expired pending record and intentionally replacing it.

Initialized submodules must be clean and checked out at their recorded gitlink.
Ouija refuses rollover preparation or adoption when an initialized submodule
has modified/untracked content or a different checked-out commit; clean or
commit that submodule first. Uninitialized submodules remain bound by the
superproject gitlink.

Continuation records live privately under Ouija's per-user data directory,
outside repositories. `ouija rollover cleanup` removes an adopted or expired
record; removing a live pending record requires the explicit
`--force-pending` override. Cleanup is a deliberate CLI action, never a
scheduled daemon job.

Do not create handoff drafts in a repository. Repository, git, test, and GitHub
evidence remains authoritative; the continuation is disposable working context
and must be checked against live state. Native subagents are not Ouija sessions:
never list, enroll, restart, or include them as Ouija descendants. Include only
exact, explicitly managed Ouija child IDs.

## 7. Task scheduling

```bash
# Create a scheduled task (cron in UTC):
ouija task add check-logs "0 9 * * *" "check error logs"

# List tasks:
ouija task list

# Trigger immediately:
ouija task trigger TASK_ID

# Remove:
ouija task remove TASK_ID
```

Tasks are for independent periodic work, not active-context refresh. Do not
schedule context audits or use `--inject-only` as a rollover/refresh mechanism.
The active-context policy above has no scheduler enrollment, child traversal,
or native-subagent discovery.

## 8. Housekeeping

**Update your metadata** when your focus changes:
```bash
ouija announce --role "what you are doing" --bulletin "what you need or offer"
```

**Clear idle reminders** — an injected `<ouija-status type="reminder">` includes
the exact clearing command for that nudge. Run that generated command verbatim;
do not invent an ID or place a clearing command in `--reminder`.

**Clear pending replies** when the sender disconnected:
```bash
ouija clear-reply SENDER_ID
```

## 9. Non-tmux contexts (opencode HTTP API, etc.)

The CLI infers your session ID from `$TMUX_PANE`. In engines whose bash tool runs outside tmux, that variable may be unset and implicit `ouija ask/tell/reply` cannot always resolve a sender automatically.

Run `ouija whoami` to learn your own id when automatic identity evidence is available. It resolves through the same signals implicit sends use, prints the id on stdout, and fails loudly with per-signal diagnostics when it cannot identify you. Implicit `ouija whoami` remains fail-closed when pane, environment, or backend identity cannot prove one Local owner.

For an explicit local send, an exact injected or operator-provided public Local session id is authoritative even when `ouija whoami` has missing, not-found, or incomplete backend evidence. Use that exact id with `--from`. The daemon requires it to name an existing Local session and rejects the send if pane, environment, or a complete backend pair positively resolves the caller to a different Local session.

Use only an exact public id as the sender: the output of `ouija whoami`, your `$OUIJA_SESSION_ID`, the id in your injected system prompt (`You are session "<id>" on the ouija mesh`), or an exact id explicitly provided by the operator. Never guess a sender id — not the project directory name, a branch name, or an entry picked from `ouija ls` (`ouija ls` shows all sessions but cannot tell you which one is you). A guessed `--from` impersonates another session and misroutes its replies.

Never use `opencode` or an OpenCode `backend_session_id` as `--from`. Those are backend implementation details, not public Ouija route targets.

Two ways to provide the public Ouija sender id explicitly:

```bash
# Per-command flag (exact public Local id, never a guess):
ouija ask target-id "question" --from public-ouija-id
ouija tell target-id "fyi" --from public-ouija-id
ouija reply target-id 47 "result" --from public-ouija-id
ouija rename new-public-id --from current-public-ouija-id

# Or set once for the shell:
export OUIJA_SESSION_ID=public-ouija-id
ouija ask target-id "question"
```

If implicit resolution fails and you do not have an exact injected or operator-provided public Local id, run `ouija whoami` and relay its diagnostics. **Never run `ouija register` to "fix" this** — it would create a duplicate session entry, not register the caller.

### Recover a running backend from a both-null row

Use this only when the operator explicitly identifies the exact public Local
session and its existing daemon row has both `backend` and
`backend_session_id` absent, while the current adapter discovers one complete
backend identity. This command is intentionally usable when `ouija whoami`
fails with backend resolution outcome `not_found`:

```bash
ouija recover-backend-identity exact-public-local-id
```

The positional value must come from trusted injected context or the operator.
Never infer it from the project, pane, process, role, or `ouija ls`. The CLI
does not put the backend-native identity in argv; it discovers the typed pair
through the current backend adapter and sends it only to the local daemon.

The daemon accepts recovery only when the target remains the same exact Local
owner with a live assistant pane, canonical matching project, exact
incarnation/owner markers, matching backend process, no lifecycle lease or
managed-launch proof, a still-blank backend pair, and an identity not claimed
elsewhere. It holds the pane/project/backend resource gates through inspection,
the blank-to-bound compare-and-swap, and durable persistence. Any mismatch or
concurrent ownership change fails closed. A successful recovery preserves the
running backend context and does not respawn; the same request cannot be
replayed because the target is no longer blank.

Do not use this for a one-sided/incomplete or already populated backend pair,
a Remote session, a missing/stale pane, or a lifecycle operation in progress.
Do not run `ouija register` as fallback. Use the existing explicit fresh
managed repair/restart when this narrow recovery refuses the row.

## 9. Patterns

Recurring recovery and completion are separate. Supplying `--reminder` opts into idle-cycle recovery nudges; `--when-done` controls what the session does after completion. Pending replies remain an independent reason to wake a session.

### Loop with termination

Two nested loops: the reminder re-injection is the inner loop (same context); `ouija restart-session --fresh` is the outer loop (clean context, same `prompt + reminder`).

```bash
ouija spawn-session counter \
  --no-parent-session --when-done keep-open \
  --prompt "read value.txt, add 1 to the number, write it back" \
  --reminder "If the number is below 10, call 'ouija restart-session counter --fresh'. If it reached 10, record that state in value.txt."
```

The reminder is the task loop's recovery context. State lives in the world (files, git, APIs), not in the session's memory, so every iteration is re-enterable from scratch. Ouija appends the concrete clearing command to each nudge. The `keep-open` completion policy leaves the session available after the loop finishes.

### Report-back when done

```bash
ouija spawn-session worker --project-dir /path --prompt "implement feature X" \
  --parent-session hub --when-done ask-parent
```

This launch receives generated ask-parent completion instructions but no recurring task reminder. Add `--reminder "Re-check task state and continue unfinished work."` only when recovery nudges are desired; Ouija appends the current clearing command.

### State-check (not state-assume) reminders

A static reminder like *"Run init to begin"* becomes noise on the second re-injection — the session already ran init. Reminders must make sense on the 5th re-injection, not just the first. Phrase them as state checks:

```
reminder: "Check state: if pending → initialize. If running → continue open work. If complete → verify and report the final state."
```

This prevents workers from treating every re-injection as the first turn. Keep
completion behavior and clearing commands out of manual reminder text; Ouija
generates both from live session state.
