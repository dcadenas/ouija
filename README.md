# ouija

Ad-hoc collaboration between live Claude Code sessions, locally or across machines. Sessions communicate through tmux injection, each stays in its own terminal, fully interactive.

You've been building the auth service in one session for hours. Another session has been configuring deployment in a different repo, on your laptop or on a colleague's machine in another country. You realize each holds context the other needs. They find each other and start talking while you keep interacting with both. No restart, no re-planning, no context lost.

![Two Claude Code sessions collaborating through ouija](screenshot.png)

Unlike Claude Code [agent teams](https://code.claude.com/docs/en/agent-teams), which plan a team upfront for a single task, ouija connects sessions that weren't planned together. Ad-hoc, cross-machine, no hierarchy. They're complementary: you can run agent teams inside ouija sessions.

## Quick start

```bash
cargo install ouija
ouija start
```

This launches the daemon and auto-configures Claude Code (MCP endpoint, hooks, skills, status line). Open Claude Code inside tmux:

```bash
tmux new-session && claude
```

Sessions auto-register using the working directory name (e.g. `/code/api` becomes `api`). Start talking:

> "Use ouija to ask api what port the auth service runs on"

## What you can do

**Message any session**, local or remote. Sessions discover each other automatically.

**Spawn sessions on the fly.** `session_start("crash-ios")` creates a tmux window, launches Claude Code, and registers it. Pass a `prompt` to seed the session with context. Works on `session_restart` too.

**Schedule tasks.** Cron jobs that inject messages into sessions. If the target session is dead, the daemon revives it automatically. One-shot tasks (`once: true`) fire once then auto-delete. Test immediately with `task_trigger`.

**Worktree sessions.** Spawn sessions in isolated git worktrees for parallel work on the same repo without branch conflicts.

**Human DMs.** Configure your Nostr npub to control the daemon from any Nostr client. Send `/list`, `/start`, `@session message`, or bare text (routed by an LLM).

**Admin dashboard** at `localhost:7880/admin`. Manage sessions, tasks, node connections, human access, and settings.

## Connecting machines

On machine A:

```bash
ouija ticket
```

On machine B:

```bash
ouija connect <ticket> --name macbook
```

Sessions on both machines discover each other. Tickets contain a connect secret, only authorized nodes can communicate.

## How it works

1. Each machine runs an **ouija daemon** (small Rust binary)
2. Sessions connect via **MCP** and auto-register on startup
3. Local messages: **tmux injection** into the target pane
4. Remote messages: **Nostr NIP-17 private DMs**, encrypted, decoupled, NAT-traversing
5. Node auth: **connect secret** in the ticket, unknown senders rejected

## Security

- **Tickets are secrets.** Share out-of-band only (copy/paste, not through Claude).
- **Connect secret auth.** Unknown senders are rejected.
- **Encrypted transport.** NIP-17 gift-wrapped DMs (NIP-44 encryption). Relays cannot read content.
- **Localhost only.** The daemon binds to `127.0.0.1`.
- **Claude never sees tickets.** MCP tools only expose session IDs and messages.

## CLI

```bash
ouija start          # start the daemon
ouija stop           # stop it
ouija update         # install latest from crates.io, restart
ouija nodes          # list self and connected nodes
ouija config ...     # manage settings, human sessions, router
```

Run `ouija --help` for the full command list.

## Data

Config in `~/.config/ouija/` (settings, identity). Data in `~/.local/share/ouija/` (sessions, tasks, connections). Message metadata is logged for diagnostics (content is not logged).

## Testing

```bash
# All tests (unit + local e2e + nostr e2e, all in Docker)
tests/e2e/run-e2e.sh

# Only local e2e
tests/e2e/run-e2e.sh local

# Only nostr P2P e2e (relay + 4 daemons + auth tests)
tests/e2e/run-e2e.sh nostr
```
