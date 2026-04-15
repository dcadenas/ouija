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

Long task — send progress first, then final result:
```bash
# Progress (resets nudge timer, doesn't clear pending reply):
ouija tell session-id "working on it" --reply-to 47

# Final result (clears pending reply):
ouija reply session-id 47 "done: here is the result"
```

## 2. Discovering sessions

```bash
ouija ls
```

Shows each session's id, role, project_dir, bulletin, and whether its metadata is stale.

## 3. Sending messages proactively

```bash
# Ask a question (expects reply):
ouija ask target-id "question"

# Inform (fire-and-forget):
ouija tell target-id "fyi: deploy done"
```

## 4. Starting and managing sessions

```bash
# Start a session:
ouija spawn-session worker --project-dir /path/to/project --prompt "implement the feature" --reminder "When done: ouija tell hub \"done: summary\""

# With worktree isolation:
ouija spawn-session worker --project-dir /path --prompt "task" --worktree --branch feature --base-branch main

# Restart with fresh context:
ouija restart-session worker --fresh --prompt "new task" --reminder "when done, report back"
# prompt/reminder optional — if omitted, reuses previous values

# Kill:
ouija kill-session worker
```

Key fields:
- `--reminder` — re-injected every time the session goes idle. Use it for report-back, self-terminate, periodic checks, or escalation
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

**Clear idle reminders** — the daemon injects `<ouija-status type="reminder" clearing_id="N">` when idle:
```bash
ouija clear-reminder N
```

**Clear pending replies** when the sender disconnected:
```bash
ouija clear-reply SENDER_ID
```

## 7. Patterns

The reminder is re-injected every idle cycle and is the main knob for session control flow. Write it as code a fresh-context session can execute, not as a one-shot instruction.

### Loop with termination

Two nested loops: the reminder re-injection is the inner loop (same context); `ouija restart-session --fresh` is the outer loop (clean context, same `prompt + reminder`).

```bash
ouija spawn-session counter \
  --prompt "read value.txt, add 1 to the number, write it back" \
  --reminder "if the number is < 10, call 'ouija restart-session counter --fresh'. Otherwise: ouija tell hub 'done: counter reached 10' then ouija clear-reminder N."
```

The reminder is the control flow — a continue branch and a terminate branch. State lives in the world (files, git, APIs), not in the session's memory, so every iteration is re-enterable from scratch.

### Report-back when done

```bash
ouija spawn-session worker --project-dir /path --prompt "implement feature X" \
  --reminder "When finished: ouija tell hub \"done: <summary>\", then ouija clear-reminder N."
```

Without `clear-reminder N`, the worker keeps getting nudged forever after it signals done. The `N` comes from the `clearing_id` the daemon stamps on each re-injection.

### State-check (not state-assume) reminders

A static reminder like *"Run init to begin"* becomes noise on the second re-injection — the session already ran init. Reminders must make sense on the 5th re-injection, not just the first. Phrase them as state checks:

```
reminder: "Check state: if pending → init. If running → continue your open work. If complete → report done and ouija clear-reminder N."
```

This is the anti-pattern fix for workers that get stuck in post-success idle: the reminder reads the world on every re-injection and picks the right branch, instead of assuming it's still the start.
