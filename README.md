# ouija

Ad-hoc collaboration between live Claude Code sessions, across machines.

![Two Claude Code sessions collaborating through ouija](screenshot.png)

The left session asks ouija to find out what port the auth service runs on. The right session — which just created the config file — receives the question, reads `config.toml`, and sends the answer back. Both sessions stay in their own terminals, fully interactive.

This works across machines too. Sessions on a remote node appear as `macbook/ios` — messages travel encrypted over Nostr relays, NAT-traversing, no port forwarding needed.

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

**Message any session** — local or remote. Sessions discover each other automatically.

**Spawn sessions on the fly** — `session_start("crash-ios")` creates a tmux window, launches Claude Code, and registers it. Coordinate parallel investigations, then collect results.

**Schedule tasks** — cron jobs that inject messages into sessions. If the target session is dead, the daemon revives it automatically. One-shot tasks (`--once`) fire once then auto-delete.

**Worktree sessions** — spawn sessions in isolated git worktrees for parallel work on the same repo without branch conflicts.

**Human DMs** — configure your Nostr npub to control the daemon from any Nostr client. Send `/list`, `/start`, `@session message`, or bare text (routed by an LLM).

**Admin dashboard** — live at `http://localhost:7880/admin`. Manage sessions, tasks, node connections, human access, and settings.

## Connecting machines

On machine A:

```bash
ouija ticket
```

On machine B:

```bash
ouija connect <ticket> --name macbook
```

Sessions on both machines discover each other. Tickets contain a connect secret — only authorized nodes can communicate.

## How it works

1. Each machine runs an **ouija daemon** (small Rust binary)
2. Sessions connect via **MCP** and auto-register on startup
3. Local messages: **tmux injection** into the target pane
4. Remote messages: **Nostr NIP-17 private DMs** — encrypted, decoupled, NAT-traversing
5. Node auth: **connect secret** in the ticket — unknown senders rejected

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

Stored in `~/.local/share/ouija/`. Message metadata is logged for diagnostics (content is not logged).

## Testing

```bash
cargo test

# E2E (Docker)
docker build -f tests/e2e/Dockerfile -t ouija-test . && docker run --rm ouija-test

# Nostr P2P E2E (Docker Compose — relay + 4 daemons + auth tests)
docker compose -f tests/e2e/docker-compose.nostr.yml up --build --abort-on-container-exit
```
