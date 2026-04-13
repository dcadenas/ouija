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
