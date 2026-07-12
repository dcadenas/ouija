# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```bash
cargo build                              # dev build
cargo clippy --all-targets --all-features  # lint (run before commit)
cargo test                               # unit tests; Stateright model check is ignored by default (~5s)
cargo test model_check_bfs -- --ignored --nocapture  # explicit Stateright model check (CPU-intensive)
tests/e2e/run-e2e.sh                    # all e2e (Docker, ~2min)
tests/e2e/run-e2e.sh local              # single-daemon e2e only
```

## Release

```bash
mise run release         # bump next alpha, commit, tag, push, publish to crates.io, restart daemon
mise run release-stable  # release v0.1.0 or OUIJA_RELEASE_VERSION=x.y.z as non-prerelease
```

Never use `cargo build --release` + manual install. The mise task handles everything.

## Architecture

Ouija is a daemon that lets Claude Code sessions discover each other and exchange messages. Sessions run in tmux panes; the daemon injects messages via tmux paste-buffer.

### Core pattern: pure state machine + side effects

All session mutations flow through `DaemonState::apply(Event)` in `daemon_protocol.rs`. This function is pure (no I/O, no async) and formally verified with Stateright. Side effects (tmux injection, Nostr transport, persistence) happen separately in `AppState::execute_effects()` in `state.rs`.

When adding features: update `DaemonState::apply()` first, then wire up effects.

When adding or changing daemon events, decide whether the event should bump `SessionMeta::last_metadata_update` and document the reason near the event. User-facing metadata freshness tracks role/bulletin staleness; internal plumbing such as `AdoptBackend` must not silently change that signal (#458).

Activity signals that reset idle or watchdog timers should use the existing `SessionMsg::Active` path. Do not introduce a parallel timer-reset message unless the existing session-agent semantics are wrong (#448).

### Registration and tmux invariants

`Event::Register` owns local pane binding. `apply_register` deduplicates by pane and can evict the previous Local session for that pane, so every external registration path must apply the same defenses as the canonical scan path: only register assistant panes, skip panes already bound to Local sessions, respect the `@ouija_id` claim marker when scanning, require a usable current path/session name, normalize through `resolve_project_root` + `sanitize_session_id`, and share `resolve_unique_session_id` conflict handling instead of reimplementing it (#1442).

When spawning or respawning tmux panes for Ouija-managed sessions, route environment arguments through `tmux::pane_env_args(session_id)`. That helper exports `OUIJA_SESSION_ID` and the history-suppression variables for `new-window`, `new-session`, and `respawn-pane`; inlining `-e KEY=VALUE` at spawn sites can break `resolve_my_session_id` during the pane-var race window (#1429).

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
- **`backend/codex.rs`** — Codex CLI integration: TUI-injection delivery, `~/.codex/hooks.json` bootstrap. See `docs/codex-cli.md`.
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

### Codex CLI backend

`backend/codex.rs` adds a TUI-injection backend (`name = "codex-cli"`) that launches `codex --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={"<trust-root>"={trust_level="trusted"}}'` inside an **Ouija-managed** cwd/worktree. Key differences from Claude Code, all deliberate (#1442):

- **No Codex `--worktree`.** Codex CLI has no such flag; Ouija/Hub set up the worktree and Codex starts inside it via `cd <dir>`. Codex *app*-managed worktrees (`$CODEX_HOME/worktrees`, detached HEAD) are a separate feature and out of scope.
- **Codex effort uses config, not `--effort`.** Codex exposes no verified CLI effort flag, so Ouija maps its existing `effort` field to `-c 'model_reasoning_effort="<effort>"'` instead of guessing a flag. Model/provider selection remains **user-owned Codex config** (`-m/--model`, `--oss`, `--local-provider`); Ouija only passes `-m <model>` through when set.
- **Turn-scoped `Stop`, no hook-driven unregister.** Codex fires `Stop` after *every* turn and has no `SessionEnd` event, so the Codex Stop hook only does turn bookkeeping and returns `{"continue":true}` — it must never unregister. Session cleanup relies on pane/process liveness (`pane_alive` tree walk), which already handles the `node -> codex` npx wrapper. `scheduler::wait_for_process` is process-tree-aware for the same reason.
- **Mesh instructions: installed skill + hook context.** Codex loads skills from `~/.codex/skills`, so `install()` writes the shared `skills/ouija/SKILL.md` there (idempotent, non-clobbering). Codex tool shells can lose tmux and Ouija environment variables, so the Codex adapter presents `CODEX_THREAD_ID` through Ouija's generic backend-identity contract; the daemon compares it with the opaque session id recorded by the SessionStart hook. Because the static skill cannot know the session's live public id, `session_start_inner` also returns mesh-CLI instructions (with the public id as `--from`) in `output` for codex-cli only; the register hook wraps them into Codex SessionStart `additionalContext` (#1445).
- **Full-power worker mode.** Codex launches with `--dangerously-bypass-approvals-and-sandbox`, matching Claude Code's `bypassPermissions` posture for Ouija-managed workers. The selected Ouija/Hub worktree is cwd/scoping, not isolation; the real boundary is trust in local automation today and an external runner sandbox such as Docker in future deployments (#1445).
- **Launch-time project trust.** Codex receives `-c 'projects={"<trust-root>"={trust_level="trusted"}}'` on start/resume so the trust gate does not block tmux-injected workers. This is invocation-scoped and does not edit user config. For linked worktrees, `<trust-root>` is the common repository root derived from `git rev-parse --git-common-dir` when it ends in `.git`.
- **Hook trust.** `install()` writes/merges `~/.codex/hooks.json` idempotently. Normal installs may require Codex's hook trust-review; tests use `--dangerously-bypass-hook-trust`. See `docs/codex-cli.md`.

## Testing patterns

- **Unit tests**: inline `#[cfg(test)]` modules in each file
- **Model checking**: Stateright BFS in `daemon_protocol.rs` verifies all state machine invariants
- **E2E tests**: Docker Compose scenarios in `tests/e2e/` (single-daemon, Nostr P2P, OpenCode, install)

### Tmux isolation in unit tests

Unit tests exercise `apply_and_execute`, which dispatches `Effect::SetTmuxVar`, `Effect::RenameWindow`, etc. Those effects must NEVER reach the host tmux server, or tests will rewrite real pane vars (e.g. `@ouija_session`) and window names when `cargo test` runs inside a tmux session.

The tmux-side primitives in `src/tmux_var.rs` and `src/tmux.rs` (`rename_window`, `enable_automatic_rename`) early-return under `cfg!(test)`. Any new function that shells out to `tmux` from an effect handler must follow the same pattern.

### E2E state restoration

E2E bash tests under `tests/e2e/` run with `set -euo pipefail`, so any test that mutates shared daemon state must install a restore `trap` before the mutating request. This includes `/api/settings` toggles such as `auto_register`, timeout settings, session caps, and projects directories. Clear the trap only after the final assertion and explicit restore; otherwise a mid-test failure leaks state into later tests (#1434).
