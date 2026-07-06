# Using the Codex CLI backend with Ouija

Ouija can drive [OpenAI Codex CLI](https://developers.openai.com/codex) sessions
as first-class mesh peers, alongside Claude Code and OpenCode. Codex sessions run
in a tmux pane and receive messages via TUI paste-injection.

## How it works

`ouija start-server` bootstraps the integration by calling `Codex::install()`,
which writes hook scripts to `~/.codex/ouija-hooks/` and merges Ouija's hook
entries into `~/.codex/hooks.json` (idempotently — your own hooks are preserved).

When Ouija starts a Codex session it launches:

```
cd <project-dir> && codex --ask-for-approval never --sandbox workspace-write --no-alt-screen [--model <model>]
```

- `--ask-for-approval never --sandbox workspace-write` — bounded autonomy: no
  per-command approval prompts (which would stall tmux injection), writes confined
  to the workspace. For fully unrestricted runs in an externally-sandboxed
  environment, use `--dangerously-bypass-approvals-and-sandbox` yourself; Ouija
  does not emit it.
- `--no-alt-screen` — preserves scrollback so pane capture/debugging works.

Resuming continues the latest thread in the cwd via `codex resume --last`, or a
specific one via `codex resume <session-id>`.

## Model and provider selection is yours

Ouija does **not** pick a model or provider for Codex. It only passes `-m <model>`
through when a session explicitly sets one. Everything else — default model,
`--oss`, `--local-provider <lmstudio|ollama>`, provider API keys — is **user-owned
Codex configuration** (`~/.codex/config.toml` and Codex CLI flags). Configure Codex
the way you normally would; Ouija launches it inside the chosen project directory.

There is **no Codex `--effort` flag**, so Ouija's `effort` setting is ignored for
Codex rather than guessed onto the command line. If you want reasoning-effort
control, set it through Codex's own config.

## Worktrees are Ouija-managed, not Codex-managed

Codex CLI has no `--worktree` flag. Ouija (or Hub) sets up the worktree/branch
before launch and starts Codex inside it with `cd`. Codex's own app-managed
worktrees (under `$CODEX_HOME/worktrees`, detached HEAD by default) are a separate
feature and are **not** used by the Ouija backend. Use Ouija's `--worktree` /
`--branch` options on `spawn-session` as usual.

## Lifecycle: turn-scoped Stop, liveness-based cleanup

Codex fires a `Stop` hook after **every** assistant turn and has **no**
`SessionEnd` event. Consequences:

- The Codex `Stop` hook only performs turn bookkeeping (pending-reply / idle
  checks) and returns `{"continue":true}`. It **never unregisters** the session.
- Session cleanup is driven by **pane/process liveness**, not a hook. Ouija's
  `pane_alive` process-tree walk detects when the pane or the `codex` process
  (a descendant of the `node`/npx wrapper) is gone and reaps the session.

## Mesh instructions

Codex has no auto-loaded ouija skill. On session start, Ouija returns short
mesh-CLI instructions that the Codex `SessionStart` hook surfaces as
`additionalContext`. They teach the session to message peers with:

```
ouija ls
ouija ask <target> "question" --from <your-public-id>
ouija tell <target> "note" --from <your-public-id>
ouija reply <target> <msg-id> "answer" --from <your-public-id>
```

`--from <your-public-id>` is included because Codex's bash tool cannot be relied
on to carry `TMUX_PANE` for sender resolution.

## Hook trust

Codex reviews hooks before running them. On a normal install you may be prompted
to trust `~/.codex/hooks.json` the first time a Codex session starts — approve it
so the register/activity/stop hooks can run. Automated tests bypass this with
`--dangerously-bypass-hook-trust`; do not use that flag for interactive use.

## Availability detection

`is_available()` uses a timeout so a slow or hanging `codex --version` (Codex is
often installed via an npx/node wrapper that can stall) cannot block daemon
startup or per-session backend detection.
