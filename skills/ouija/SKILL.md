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
# prompt/reminder optional — if omitted, reuses previous values

# Kill:
ouija kill-session worker
```

Key fields:
- `--parent-session <SESSION_ID>` / `--no-parent-session` — required lifecycle ownership choice for spawned sessions
- `--when-done keep-open|ask-parent|close` — required completion behavior, independent of recurring reminders. Ouija generates the stay-open/ask-parent/close instructions
- `--idle-policy` is deprecated; legacy scripts may still use `keep-open|ask-parent-when-done|close-when-done`
- `--reminder` alone opts the session into recurring recovery nudges. Omit it for no task-reminder recurrence
- Pending replies can still wake a session without `--reminder`.
- Never put `ouija clear-reminder` in manual reminder text. Ouija adds the concrete clearing command and ID to each injected nudge
- `--worktree` — isolate in a git worktree at `~/.ouija/worktrees/<repo>/<session>`
- `--branch` / `--base-branch` — git branch control for worktrees

## 5. Task scheduling

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

## 6. Housekeeping

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

## 7. Non-tmux contexts (opencode HTTP API, etc.)

The CLI infers your session ID from `$TMUX_PANE`. In engines whose bash tool runs outside tmux, that variable is unset and `ouija ask/tell/reply` cannot resolve a sender automatically.

Run `ouija whoami` to learn your own id. It resolves through the same signals the send commands use, prints the id on stdout, and fails loudly with per-signal diagnostics when it cannot identify you.

Use only an exact id as the sender: the output of `ouija whoami`, your `$OUIJA_SESSION_ID`, or the id in your injected system prompt (`You are session "<id>" on the ouija mesh`). Never guess a sender id — not the project directory name, a branch name, or an entry picked from `ouija ls` (`ouija ls` shows all sessions but cannot tell you which one is you). A guessed `--from` impersonates another session and misroutes its replies; the daemon rejects claims it can disprove, but only an exact id is safe.

Never use `opencode` or an OpenCode `backend_session_id` as `--from`. Those are backend implementation details, not public Ouija route targets.

Two ways to provide the public Ouija sender id explicitly:

```bash
# Per-command flag (id from `ouija whoami`, never a guess):
ouija ask target-id "question" --from public-ouija-id
ouija tell target-id "fyi" --from public-ouija-id
ouija reply target-id 47 "result" --from public-ouija-id

# Or set once for the shell:
export OUIJA_SESSION_ID=public-ouija-id
ouija ask target-id "question"
```

If you see an error about being unable to resolve the current session ID, run `ouija whoami` and follow its diagnostics. **Never run `ouija register` to "fix" this** — it would create a duplicate session entry, not register the caller.

## 8. Patterns

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
