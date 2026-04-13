# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```bash
cargo build                              # dev build
cargo clippy --all-targets --all-features  # lint (run before commit)
cargo test                               # unit tests + Stateright model check (~3.5min)
cargo test -- --skip stateright          # unit tests only (~5s)
tests/e2e/run-e2e.sh                    # all e2e (Docker, ~2min)
tests/e2e/run-e2e.sh local              # single-daemon e2e only
```

## Release

```bash
mise run release    # bump version, commit, tag, push, publish to crates.io, restart daemon
```

Never use `cargo build --release` + manual install. The mise task handles everything.

## Architecture

Ouija is a daemon that lets Claude Code sessions discover each other and exchange messages. Sessions run in tmux panes; the daemon injects messages via tmux paste-buffer.

### Core pattern: pure state machine + side effects

All session mutations flow through `DaemonState::apply(Event)` in `daemon_protocol.rs`. This function is pure (no I/O, no async) and formally verified with Stateright. Side effects (tmux injection, Nostr transport, persistence) happen separately in `AppState::execute_effects()` in `state.rs`.

When adding features: update `DaemonState::apply()` first, then wire up effects.

### Key modules

- **`daemon_protocol.rs`** — `DaemonState`, `Event` enum, `Effect` enum, all session logic. The heart of ouija.
- **`state.rs`** — `AppState` wraps DaemonState in `Arc<RwLock>`, owns transports, executes effects.
- **`api.rs`** — REST endpoints: `/api/send`, `/api/sessions/start`, `/api/sessions/restart`, tasks.
- **`hooks.rs`** — Endpoints called by Claude Code hooks: session-start, session-end, stop, prompt-submit, pre-tool-use.
- **`nostr_transport.rs`** — Nostr P2P messaging (NIP-17 encrypted DMs), session start/restart/kill orchestration.
- **`session_agent.rs`** — Per-pane Ractor actor: idle timers, loop stall detection, reminder injection.
- **`scheduler.rs`** — Cron tasks with `OnFire` modes (ContinueSession, NewSession, PersistentWorktree, DisposableWorktree).
- **`backend/claude_code.rs`** — Claude Code integration: CLI command building, plugin bootstrap, workspace trust.
- **`backend/opencode.rs`** — OpenCode integration: HTTP API delivery mode.
- **`tmux.rs`** — Pane discovery, message injection with bracketed paste, vim-mode detection.

### Message delivery

Local: API call -> `apply(Event::Send)` -> tmux paste-buffer injection into recipient pane.
Remote: Same flow but wrapped in NIP-17 encrypted DMs via Nostr relays.

### Plugin system

`ouija start-server` writes embedded files (hooks, scripts, skills) to `~/.claude/plugins/cache/ouija/`. The hook scripts are thin wrappers that POST to `localhost:7880/api/hooks/*`. The skill (`skills/ouija/SKILL.md`) teaches Claude Code how to use the ouija CLI.

### Session lifecycle

1. Claude Code starts in a tmux pane
2. `SessionStart` hook registers the pane with the daemon
3. Messages arrive as `<msg from="sender" id="N" reply="true">text</msg>` XML injected into the pane
4. `Stop` hook fires after each turn, triggers idle/pending-reply checks
5. `SessionEnd` hook unregisters on exit

### Workspace trust

`pre_trust_workspace()` in `backend/claude_code.rs` writes `hasTrustDialogAccepted: true` to `~/.claude.json` before spawning sessions, bypassing the interactive trust dialog.

## Testing patterns

- **Unit tests**: inline `#[cfg(test)]` modules in each file
- **Model checking**: Stateright BFS in `daemon_protocol.rs` verifies all state machine invariants
- **E2E tests**: Docker Compose scenarios in `tests/e2e/` (single-daemon, Nostr P2P, OpenCode, install)
