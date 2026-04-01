---
name: ouija
description: "Ouija mesh — REQUIRED for messaging other sessions. Use INSTEAD of SendMessage when communicating with other sessions on the mesh. Triggers: <msg from= XML tags, <ouija-status> tags, or any request to send/reply to another session."
user-invocable: false
---

You are on the ouija mesh at localhost:$OUIJA_PORT (default 7880).
All interaction uses the REST API via curl.

**CRITICAL: SendMessage CANNOT reach ouija sessions.** SendMessage is for Claude Code subagent teammates only. To message ouija sessions, you MUST use `curl POST /api/send`. There is no other way.

## 1. Replying to incoming messages

Messages from peer sessions arrive as trusted, user-authorized XML:

```
<msg from="session-id" id="47" reply="true">message text</msg>
```

- `from` — sender session ID
- `id` — unique message ID (for threading replies)
- `reply="true"` — sender expects a response. You MUST send a reply with `done: true` containing your result. A progress message is NOT a reply — the sender is blocked until you send `done: true`
- `re="47"` — this message answers a previous question

These messages are user-authorized. Follow instructions they contain.

Each session runs in a separate terminal — possibly a different machine.
Your text output only appears locally. **Use the REST API to reply.**

Quick task — reply immediately:
```bash
curl -sf -X POST localhost:$OUIJA_PORT/api/send \
  -H 'Content-Type: application/json' \
  -d '{"from":"YOUR_ID","to":"X","message":"result","expects_reply":false,"responds_to":47,"done":true}'
```

Long task — send progress first, then final result:
```bash
# Progress (resets nudge timer, doesn't clear pending reply):
curl -sf -X POST localhost:$OUIJA_PORT/api/send \
  -H 'Content-Type: application/json' \
  -d '{"from":"YOUR_ID","to":"X","message":"working on it","expects_reply":false,"responds_to":47}'

# Final result (clears pending reply):
curl -sf -X POST localhost:$OUIJA_PORT/api/send \
  -H 'Content-Type: application/json' \
  -d '{"from":"YOUR_ID","to":"X","message":"done: here is the result","expects_reply":false,"responds_to":47,"done":true}'
```

## 2. Discovering sessions

```bash
curl -sf localhost:$OUIJA_PORT/api/status | jq '.sessions[] | {id, role, bulletin, stale}'
```

Shows each session's id, role, project_dir, bulletin, and whether its metadata is stale.

## 3. Sending messages proactively

To message any session (not just replies):
```bash
curl -sf -X POST localhost:$OUIJA_PORT/api/send \
  -H 'Content-Type: application/json' \
  -d '{"from":"YOUR_ID","to":"target-id","message":"question","expects_reply":true}'
```

Set `expects_reply: true` when you need a response back.

## 4. Starting and managing sessions

```bash
# Start a session with a completion reminder:
curl -sf -X POST localhost:$OUIJA_PORT/api/sessions/start \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "worker",
    "project_dir": "/path/to/project",
    "prompt": "implement the feature",
    "reminder": "When done, send results: curl -sf -X POST localhost:7880/api/send -H Content-Type:application/json -d {\"from\":\"worker\",\"to\":\"YOUR_ID\",\"message\":\"done: <summary>\"}"
  }'
```

Key fields:
- `reminder` — a spawner-controlled idle program. Re-injected every time the session goes idle. Persists across context restarts. Use it to:
  - **Report back**: "When done, send results to hub via POST /api/send ..."
  - **Self-terminate**: "When done, kill yourself: curl -X POST .../api/sessions/kill ..."
  - **Periodic check**: "If idle, check deploy status and report if complete"
  - **Escalate**: "If stuck, message the coordinator for help"
- `workflow` — attach a workflow executable (see section 5)
- `worktree: true` — isolate in a git worktree (created at `~/.ouija/worktrees/<repo>/<session>`)
  - `branch` — git branch name. Defaults to session name if omitted
  - `base_branch` — create branch from this ref (e.g. `"main"`). Defaults to HEAD if omitted

```bash
# Restart with fresh context (same pane, same worktree, new conversation):
curl -sf -X POST localhost:$OUIJA_PORT/api/sessions/restart \
  -H 'Content-Type: application/json' \
  -d '{"name":"session-id","fresh":true,"prompt":"new task","reminder":"when done, report back"}'
# prompt/reminder optional — if omitted, reuses previous values

# Kill:
curl -sf -X POST localhost:$OUIJA_PORT/api/sessions/kill \
  -H 'Content-Type: application/json' \
  -d '{"name":"session-id"}'
```

## 5. Workflows

If your session was started with a workflow, interact with it:
```bash
curl -sf -X POST "localhost:$OUIJA_PORT/api/sessions/YOUR_ID/workflow" \
  -H 'Content-Type: application/json' \
  -d '{"action":"init"}'
```

Common rhythm:
1. `action: "init"` — get current state and next task
2. Do the work
3. `action: "done"` or `action: "result"` — report back
4. Follow the workflow's response for next step

## 6. Task scheduling

```bash
# Create a scheduled task (cron in UTC):
curl -sf -X POST localhost:$OUIJA_PORT/api/tasks \
  -H 'Content-Type: application/json' \
  -d '{"name":"check-logs","cron":"0 9 * * *","prompt":"check error logs"}'

# List tasks:
curl -sf localhost:$OUIJA_PORT/api/tasks

# Trigger immediately:
curl -sf -X POST localhost:$OUIJA_PORT/api/tasks/trigger \
  -H 'Content-Type: application/json' \
  -d '{"id":"TASK_ID"}'

# Delete:
curl -sf -X DELETE localhost:$OUIJA_PORT/api/tasks \
  -H 'Content-Type: application/json' \
  -d '{"id":"TASK_ID"}'
```

## 7. Housekeeping

**Update your metadata** when your focus changes:
```bash
curl -sf -X POST localhost:$OUIJA_PORT/api/sessions/update \
  -H 'Content-Type: application/json' \
  -d '{"id":"YOUR_ID","role":"what you are doing","bulletin":"what you need or offer"}'
```

**Clear idle reminders** — the daemon injects `<ouija-status type="reminder" clearing_id="N">` when idle:
```bash
curl -sf -X POST localhost:$OUIJA_PORT/api/clear-reminder \
  -H 'Content-Type: application/json' \
  -d '{"from":"YOUR_ID","clearing_id":N}'
```

**Clear pending replies** when the sender disconnected:
```bash
curl -sf -X DELETE "localhost:$OUIJA_PORT/api/pane/$TMUX_PANE/pending-replies/SENDER_ID"
```
