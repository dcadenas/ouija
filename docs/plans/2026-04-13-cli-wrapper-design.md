# CLI Wrapper Design Spec

Thin CLI subcommands on the `ouija` binary that talk to the running daemon,
replacing curl calls in SKILL.md and session workflows.

## Motivation

Each curl call costs ~40 tokens of boilerplate (method, headers, JSON body,
escaping). A CLI wrapper like `ouija ask hub "question"` costs ~5 tokens.
Sessions make dozens of API calls per conversation. This is the single
biggest token-cost lever available.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Messaging verbs | `ask`/`tell`/`reply` (not `send`) | Verb encodes intent; no flags for common case |
| `ask` default | `expects_reply=true` | You're asking — you want an answer |
| `tell` default | `expects_reply=false` | You're informing — fire and forget |
| `reply` default | `done=true`, `expects_reply=false` | You're answering — clear the pending reply |
| Metadata update | `ouija announce` | Broadcast metaphor fits; `update` was taken |
| Session lifecycle | `spawn-session`/`kill-session`/`restart-session` | Self-documenting; infrequent so extra tokens fine |
| Daemon lifecycle | `start-server`/`stop-server` | Disambiguates from session lifecycle |
| Discovery | `ouija ls` (top-level) | Highest frequency discovery command |
| Namespace | Flat top-level for frequent ops, self-documenting names for rare ops | Optimize hot path tokens, clarity for cold path |
| Output format | Raw JSON always | LLMs parse JSON fine; no format flags needed |
| Identity | Auto-detect via `@ouija_session` tmux var, fallback to API | Fast (no HTTP), authoritative (daemon manages the var) |
| SKILL.md | Full rewrite, drop all curl | Same binary = CLI always available |
| Backwards compat | None required | Private project, single user |

## Command Reference

### Tier 1: Hot Path (short, frequent)

```
ouija ask <to> <message>
```
Send a message expecting a reply. Auto-detects `from` via tmux var.
Maps to `POST /api/send` with `expects_reply=true`.

```
ouija tell <to> <message> [--reply-to <msg_id>]
```
Send a message not expecting a reply. `--reply-to` threads it as a progress
update without clearing the pending reply.
Maps to `POST /api/send` with `expects_reply=false`.

```
ouija reply <to> <msg_id> <message> [--no-done] [--expect-reply]
```
Reply to a message. Defaults to `done=true` (clears pending reply).
`--no-done` for progress that threads but doesn't complete.
`--expect-reply` if your reply contains a follow-up question.
Maps to `POST /api/send` with `responds_to=<msg_id>`, `done=true`.

```
ouija ls
```
List sessions with id, role, bulletin, stale status.
Maps to `GET /api/status`, extracts session list.

```
ouija announce [--role <text>] [--bulletin <text>]
```
Update own session metadata. Auto-detects session ID. At least one flag required.
Maps to `POST /api/sessions/update`.

```
ouija rename <new_name>
```
Rename current session. Auto-detects current session ID.
Maps to `POST /api/rename`.

### Tier 2: Session Lifecycle (self-documenting, infrequent)

```
ouija spawn-session <name> [--project-dir <path>] [--prompt <text>]
    [--reminder <text>] [--worktree] [--branch <name>] [--base-branch <name>]
    [--model <model>] [--backend <backend>] [--from <session>]
```
Start a new session. Maps to `POST /api/sessions/start`.

```
ouija kill-session <name> [--keep-worktree]
```
Kill a running session. Maps to `POST /api/sessions/kill`.

```
ouija restart-session <name> [--fresh] [--prompt <text>] [--reminder <text>]
```
Restart a session. `--fresh` clears context. Omitted `--prompt`/`--reminder`
reuse previous values. Maps to `POST /api/sessions/restart`.

### Tier 3: Housekeeping

```
ouija clear-reminder <clearing_id>
```
Clear an idle reminder. Auto-detects `from`.
Maps to `POST /api/clear-reminder`.

```
ouija clear-reply <sender_id>
```
Clear a pending reply from a disconnected sender. Uses `$TMUX_PANE`.
Maps to `DELETE /api/pane/{pane}/pending-replies/{sender}`.

### Tier 4: Admin / Infrastructure (renamed for clarity)

```
ouija start-server [--port <n>] [--name <s>] [--data <dir>] [--ticket <t>] [--relay <url>...]
ouija stop-server
ouija self-update
ouija status
ouija register <id> [pane] [--vim-mode] [--project-dir <p>] [--role <r>]
ouija unregister <id>
ouija inject <pane> <message>
ouija log-path [--data <dir>]
ouija nodes
ouija connect <ticket> [--name <s>]
ouija disconnect <node>
ouija ticket [--relay <url>...]
ouija regenerate-ticket [--yes]
ouija config [set <key> <value> | add-human | remove-human | list-humans | set-router | remove-router]
ouija task [list | add <name> <cron> <message> | remove <id> | enable <id> | disable <id> | runs | trigger <id>]
```

## Identity Resolution

Order of resolution for the caller's session ID:

1. Read `@ouija_session` tmux pane variable: `tmux display -p -t $TMUX_PANE '#{@ouija_session}'`
2. If empty/unset, query `GET /api/status` and match by `$TMUX_PANE`
3. If still unresolved, error: "no session registered for this pane"

Port resolution: `$OUIJA_PORT` env var, default `7880`.

No `--from` flag on messaging commands. Identity is always auto-detected.
This eliminates wrong-identity bugs entirely.

## SKILL.md Rewrite

Full replacement — drop all curl examples. New structure:

1. **Replying to messages** — `ouija reply`
2. **Discovering sessions** — `ouija ls`
3. **Sending messages** — `ouija ask` / `ouija tell`
4. **Session lifecycle** — `ouija spawn-session` / `kill-session` / `restart-session`
5. **Task scheduling** — `ouija task add/list/trigger/remove`
6. **Housekeeping** — `ouija announce` / `clear-reminder` / `clear-reply`

Drop the "SendMessage CANNOT reach ouija sessions" warning — sessions learn
CLI as the natural way. No curl to confuse with SendMessage.

Reminder templates injected by the daemon also change from curl to CLI:
```
# Before:
"reminder": "When done: curl -sf -X POST localhost:7880/api/send -H Content-Type:application/json -d {\"from\":\"worker\",\"to\":\"hub\",\"message\":\"done\"}"

# After:
"reminder": "When done: ouija tell hub \"done: <summary>\""
```

## Implementation Approach

Extend existing subcommands in `main.rs`. Reuse `cli_post`/`cli_get`/
`resolve_my_session_id` infrastructure. Upgrade `resolve_my_session_id` to
read the tmux var first.

Rename existing commands (`Start` -> `StartServer`, `Stop` -> `StopServer`,
`Send` -> removed, `Remove` -> `Unregister`, `Update` -> `SelfUpdate`).

No new modules or crate restructuring — this is adding/modifying clap
variants and thin match arms that call `cli_post`/`cli_get`.

## Token Cost Comparison

| Operation | curl (tokens) | CLI (tokens) | Savings |
|-----------|--------------|-------------|---------|
| Reply with done | ~40 | ~6 | 85% |
| Proactive ask | ~35 | ~5 | 86% |
| List sessions | ~15 | ~2 | 87% |
| Update metadata | ~30 | ~6 | 80% |
| Start session | ~50 | ~10 | 80% |
| Clear reminder | ~25 | ~3 | 88% |

At ~20 API calls per conversation, this saves ~500-600 tokens per session.
