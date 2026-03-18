# ouija

When you're running Claude Code in multiple terminals, they can't share what they've learned. Ouija lets them find each other and talk, even across machines.

You've been building the auth service in one session for hours. Another session has been configuring deployment in a different repo, on your laptop or on a colleague's machine in another country. You realize each holds context the other needs. They find each other and start talking while you keep interacting with both. No restart, no re-planning, no context lost.

![Two Claude Code sessions collaborating through ouija](screenshot.png)

Unlike Claude Code [agent teams](https://code.claude.com/docs/en/agent-teams), which plan a team upfront for a single task, ouija connects sessions that weren't planned together. Ad-hoc, cross-machine, no hierarchy. They're complementary: you can run agent teams inside ouija sessions.

## Prerequisites

[tmux](https://github.com/tmux/tmux) and [Claude Code](https://docs.anthropic.com/en/docs/claude-code) on your PATH.

## Quick start

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/dcadenas/ouija/releases/latest/download/ouija-installer.sh | sh
ouija start
```

Or with Rust: `cargo binstall ouija` / `cargo install ouija`.

This launches the daemon and auto-configures Claude Code (MCP endpoint, hooks, skills, status line). Open Claude Code inside tmux:

```bash
tmux new-session && claude
```

Sessions auto-register using the working directory name (e.g. `/code/api` becomes `api`). Start talking:

> "Use ouija to ask api what port the auth service runs on"

## What you can do

**Message any session**, local or remote. Sessions discover each other automatically.

**Spawn sessions on the fly.** `session_start("crash-ios")` creates a tmux window, launches Claude Code, and registers it. Pass a `prompt` to seed the session with context. Works on `session_restart` too.

**Run long-lived sessions.** Sessions persist across daemon restarts, get auto-revived by scheduled tasks, and maintain their names and context. A session that's been investigating a memory leak for hours keeps all that context available to other sessions that discover it later.

**Schedule tasks.** Cron jobs that inject messages into sessions. If the target session is dead, the daemon revives it automatically. Use this for daily reports, periodic checks, or recurring maintenance. One-shot tasks (`once: true`) fire once then auto-delete.

**Worktree sessions.** Spawn sessions in isolated git worktrees for parallel work on the same repo without branch conflicts.

**Nostr DMs.** If you use Nostr, configure your npub to control the daemon from any Nostr client. Send `/list`, `/start`, `@session message`, or bare text (routed by an LLM).

**Dashboard** at `localhost:7880`. Manage sessions, tasks, node connections, and settings.

## Connecting machines

On machine A:

```bash
ouija ticket
```

On machine B:

```bash
ouija connect <ticket> --name macbook
```

Sessions on both machines discover each other. Tickets contain a connect secret, only authorized nodes can communicate. After connecting, both nodes remember each other and auto-reconnect on restart.

## Message protocol

Sessions communicate through XML messages injected into tmux panes:

```xml
<msg from="auth" id="47" reply="true">what port does the gateway use?</msg>
```

Messages can reference earlier ones for conversation threading:
- `re="47"` — progress update on task 47
- `re="47" done="true"` — task 47 is complete

The daemon assigns unique IDs to every message, tracks pending replies, and nudges sessions that haven't responded. Sessions interact with the protocol through MCP tools (`session_send`, `session_list`, etc.) — the XML is handled automatically.

## How it works

1. Each machine runs an **ouija daemon** (small Rust binary)
2. Sessions connect via **MCP** and auto-register on startup
3. Local messages: **tmux injection** into the target pane
4. Remote messages: **end-to-end encrypted**, works across NATs without port forwarding (uses [Nostr](https://nostr.com) relays as transport)
5. Node auth: **connect secret** in the ticket, unknown senders rejected

All session state transitions go through a pure state machine (`DaemonProtocol`) that's [formally verified](tests/model/main.rs) using [Stateright](https://github.com/stateright/stateright) model checking.

## Security

- **Tickets are secrets.** Share out-of-band only (copy/paste, not through Claude).
- **Connect secret auth.** Unknown senders are rejected.
- **Encrypted transport.** End-to-end encrypted via Nostr ([NIP-17](https://github.com/nostr-protocol/nips/blob/master/17.md) gift-wrapped DMs). Relays cannot read content.
- **Localhost only.** The daemon binds to `127.0.0.1`.
- **Claude never sees tickets.** MCP tools only expose session IDs and messages.

## CLI

```bash
ouija start          # start the daemon
ouija stop           # stop it
ouija update         # install latest from crates.io, restart
ouija nodes          # list self and connected nodes
ouija config ...     # manage settings, Nostr DM users, router
```

Run `ouija --help` for the full command list.

## Data

Config in `~/.config/ouija/` (settings, identity). Data in `~/.local/share/ouija/` (sessions, tasks, connections). Message metadata is logged for diagnostics (content is not logged).

## Tmux integration

Windows are automatically named after the ouija session when the pane is the only one in the window. Each pane also gets a `@ouija_session` user variable you can use in your tmux config for more control:

```tmux
set -g window-status-current-format '#{?@ouija_session,⊕ #{@ouija_session},#{b:pane_current_path}}'
```

Fuzzy session pickers that read tmux's display format will show ouija session names automatically. The author uses [dcadenas/tmux-sessionizer](https://github.com/dcadenas/tmux-sessionizer), a fork that expands all sessions into window-level entries (e.g. `ouija/1:⊕ daily-report`), making ouija sessions easy to find and switch to.

## Testing

```bash
# All tests (unit + local e2e + nostr e2e + install, all in Docker)
tests/e2e/run-e2e.sh

# Only local e2e
tests/e2e/run-e2e.sh local

# Only nostr P2P e2e (relay + 4 daemons + auth tests)
tests/e2e/run-e2e.sh nostr

# Install/preflight tests (clean machine, no Rust)
tests/e2e/run-e2e.sh install
```
