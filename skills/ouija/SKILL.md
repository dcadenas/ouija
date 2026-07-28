---
name: ouija
description: "Ouija mesh — REQUIRED for messaging other sessions. You MUST invoke this skill via the Skill tool when you see <msg from= XML tags, <ouija-status> tags, or any request to send/reply to another session. Use INSTEAD of SendMessage — SendMessage CANNOT reach ouija sessions."
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

Shows a compact discovery list for choosing message targets. Each session includes `id` and `origin`, plus `project` (basename only), `role`, and `bulletin` when available. Use `ouija status` for full debug metadata such as absolute project paths, stale metadata, and worktree state.

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

# Restart with fresh context:
ouija restart-session worker --fresh --prompt "new task" --reminder "when done, report back"
# --prompt replaces the stored startup prompt. If omitted, the stored prompt is reused.

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
- On restart, `--prompt` is a persistent replacement; `--suppress-stored-prompt` only suppresses fallback for that launch; `--one-shot-file` appends launch-only UTF-8 content. `--backend` explicitly selects the restart backend
- Pending replies can still wake a session without `--reminder`.
- Never put `ouija clear-reminder` in manual reminder text. Ouija adds the concrete clearing command and ID to each injected nudge
- `--worktree` — isolate in a git worktree at `~/.ouija/worktrees/<repo>/<session>`
- `--branch` / `--base-branch` — git branch control for worktrees

## 5. Intentional context rollover

Use rollover only at a safe work boundary. The running session, not the Ouija
daemon, decides when a bounded slice can stop. Prepare a concise continuation
directly on stdin:

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

## 6. Task scheduling

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

For production context audits, the operator enrolls each intended manual root
with one recurring exact-target task:

```bash
ouija task add context-audit "*/15 * * * *" \
  "At your next safe boundary, audit your context. Exact Ouija child allowlist: turnero, review-plugin-cx. Send each at most one ordinary audit message." \
  --target hub-cx --inject-only
```

`--inject-only` requires an exact target and fails closed unless that exact
Local Ouija session is currently live. It never creates, revives, restarts, or
respawns a session and never touches a worktree or stored prompt. An audit is a
safe-boundary request, not a forced rollover; the session still owns the
semantic decision to continue or roll over.

The task message is the operator-owned enrollment record. It must carry the
exact allowlist of explicitly managed Ouija children. The root may corroborate
each allowlisted ID with `parent_session` from `ouija ls` or `ouija status`,
then send that exact live child at most one ordinary `ouija tell` audit. A null
or missing parent never discovers a child and never de-enrolls an explicit
operator choice. Do not create per-child schedules, recurse through children,
inspect processes, infer enrollment from names/roles/paths, or add a daemon
enrollment graph. Native subagents are not Ouija sessions and must remain
invisible. OuijaCP lifecycle semantics are unrelated and must not be inferred
from this procedure.

## 7. Housekeeping

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

## 8. Non-tmux contexts (opencode HTTP API, etc.)

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
