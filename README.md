# ouija

A daemon that bridges Claude Code sessions across machines through direct message injection. Both human and AI sessions interact in a fully decoupled way — the receiving session has no idea its input came from another AI on a different continent. Remote connections travel as Nostr private DMs, encrypted end-to-end.

## Why

Claude Code already has agents and experimental teams — but those are planned upfront, tightly scoped, and gone when the task ends. Ouija does something different: it lets you connect sessions **after the fact**, mid-conversation, without either side being designed for it.

A Claude working on your API backend can ask the one on the frontend "what's the auth token format?" and get an answer — even while you're in the middle of a conversation with either of them. The message just appears in the target's input, it responds naturally, and the reply comes back. No orchestrator, no shared context, no setup. Sessions that were started independently, on different machines, hours apart, can collaborate on the fly.

Think Slack for AI sessions, not a task queue.

## How it works

1. Each machine runs an **ouija daemon** (a small Rust binary)
2. Claude Code sessions connect via **MCP** and auto-register on startup
3. Local messages are delivered by **tmux injection** into the target pane
4. Remote messages travel as **Nostr NIP-17 private DMs** through relays — encrypted, decoupled, NAT-traversing
5. Peers authenticate with a **connect secret** embedded in the ticket — unknown senders are rejected

## Quick start

```bash
cargo install ouija
ouija setup
```

This installs the binary, starts the daemon, and registers the MCP with Claude Code.

Then open Claude Code in tmux:

```bash
tmux new-window && claude
```

Sessions auto-register on startup (named after the working directory). That's it — start talking:

> "Tell api to check the auth logs"

## Connecting two machines

On machine A:

```bash
ouija ticket
# prints: nprofile1qqsr...#a1b2c3d4e5f6...
```

On machine B:

```bash
ouija connect <ticket> --name macbook
```

Done. Sessions on both machines discover each other automatically. The ticket includes a connect secret — only peers who present it are authorized.

```bash
ouija peers
# NAME         STATUS       SINCE
# macbook      connected    14:30:05
```

## MCP tools

Claude Code sessions get three tools automatically:

| Tool | Description |
|------|-------------|
| `peer_list` | See all sessions across all connected daemons |
| `peer_send` | Send a message to any session by ID |
| `peer_register` | Re-register with a custom name, set metadata |

### Example conversation

**You** (talking to your `web` session):
> "Ask api what port the auth service runs on"

**Claude (`web`)** calls:
```
peer_send(from: "web", to: "api", message: "what port is the auth service on?")
```

**Claude (`api`)** sees `[from web]: what port is the auth service on?` appear in its input and responds:
```
peer_send(from: "api", to: "web", message: "Auth service is on port 8443, configured in config/services.yaml")
```

**Claude (`web`)** tells you the answer. Neither session needed to be designed to work together.

### Cross-machine example

Sessions on remote daemons appear with a prefix:

```
peer_send(from: "web", to: "macbook/ios", message: "what's the bundle ID?")
```

The message travels over Nostr DMs to the other machine and gets injected into the `ios` session's tmux pane.

## CLI reference

```bash
# Daemon
ouija start                          # start (default: port 7880, hostname as name)
ouija start --relay wss://nos.lol    # start with additional relay
ouija stop                           # stop the daemon
ouija update                         # install latest from crates.io, restart

# Sessions
ouija status                         # show sessions, peers, transports
ouija register <id>                  # register a session
ouija rename <old> <new>             # rename a session
ouija remove <id>                    # remove a session

# Messaging
ouija send <id> <msg>                # send a message to a session
ouija inject <pane> <msg>            # inject directly into a tmux pane

# Peering
ouija ticket                         # print connection ticket
ouija connect <ticket>               # connect to a remote daemon
ouija connect <ticket> --name mac    # connect and name the peer
ouija regenerate-ticket              # new identity + secret, drops all peers
ouija peers                          # list connected and saved peers

# Settings
ouija config                         # view settings
ouija config set auto_register false # disable auto-registration
ouija log-path                       # print message log path
```

## Dashboard

Open `http://localhost:7880/admin` for a live view of sessions, peers, and transport status.

## Security

- **Tickets are secrets.** A ticket contains the Nostr identity (nprofile) and a connect secret. Only share them out-of-band (copy/paste, not through Claude sessions).
- **Connect secret authentication.** Peers must present the correct secret to be authorized. Unknown senders are rejected.
- **Encrypted transport.** Messages travel as NIP-17 gift-wrapped DMs (NIP-44 encryption, NIP-59 gift wrap). The relay cannot read message content.
- **Localhost only.** The daemon binds to `127.0.0.1` — the API and MCP endpoint are not exposed to the network.
- **Claude never sees tickets.** MCP tools only expose session IDs and messages. Pairing is a human/CLI/dashboard operation.
- **Regenerate anytime.** `ouija regenerate-ticket` creates a new identity and secret, invalidating all existing peer connections.

## Data directory

Stored in `~/.local/share/ouija/` (or `--data <path>`):

| File | Purpose |
|------|---------|
| `nostr_nsec` | Nostr identity key (nsec) |
| `connect_secret` | Connect secret for peer authentication |
| `nostr_relays.json` | Persisted relay URLs |
| `sessions.json` | Local sessions (restored on restart) |
| `connections.json` | Saved peer connections (auto-reconnect) |
| `settings.json` | User settings |
| `messages.jsonl` | Message log (metadata only, no content) |

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `OUIJA_PORT` | `7880` | Port for CLI commands |
| `RUST_LOG` | `ouija=info` | Log level |

## Testing

```bash
cargo test                           # unit tests

# E2E (Docker)
docker build -f tests/e2e/Dockerfile -t ouija-test . && docker run --rm ouija-test

# Nostr P2P E2E (Docker Compose — includes relay + 4 daemons + auth tests)
docker compose -f tests/e2e/docker-compose.nostr.yml up --build --abort-on-container-exit
```
