# Using the Codex CLI backend with Ouija

Ouija can drive [OpenAI Codex CLI](https://developers.openai.com/codex) sessions
as first-class mesh peers, alongside Claude Code and OpenCode. Codex sessions run
in a tmux pane and receive messages via TUI paste-injection.

## How it works

`ouija start-server` bootstraps the integration by calling `Codex::install()`,
which:

- writes hook scripts to `~/.codex/ouija-hooks/` and merges Ouija's hook entries
  into `~/.codex/hooks.json` (idempotently — your own hooks are preserved), and
- installs the shared ouija skill to `~/.codex/skills/ouija/SKILL.md` (idempotent;
  unrelated skills under `~/.codex/skills/` are left untouched).

If Codex model routes are configured, `start-server` also installs the same
Codex hooks and skill into each routed Codex home.

When Ouija starts a Codex session it launches:

```
cd <project-dir> && codex --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={"<trust-root>"={trust_level="trusted"}}' [--model <model>]
```

With a Codex model route, the public session input stays `--model <alias>`, but
Ouija resolves that alias to the configured Codex launch details. For example,
`--model gemini` can resolve to `--model gemini-2.5-pro` plus an isolated
`CODEX_HOME`:

```
cd <project-dir> && CODEX_HOME=<path> codex --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={"<trust-root>"={trust_level="trusted"}}' [--model <model>]
```

- `--dangerously-bypass-approvals-and-sandbox` — full-power worker mode: no
  per-command approval prompts and no Codex sandbox boundary. Ouija still
  launches Codex inside the selected Ouija/Hub worktree, but that worktree is
  cwd/scoping, not isolation. This matches Claude Code's `bypassPermissions`
  worker posture. It is intended for trusted local automation now and for
  deployments where Ouija itself runs inside an external sandbox boundary, such
  as Docker.
- `--no-alt-screen` — preserves scrollback so pane capture/debugging works.
- `-c 'projects={"<trust-root>"={trust_level="trusted"}}'` — skips Codex's
  project trust prompt for this invocation without mutating `~/.codex/config.toml`.
  Codex expects the whole `projects` table override here; dotted `-c
  'projects."/path".trust_level="trusted"'` forms did not bypass the prompt in
  live testing. For linked Git worktrees, Codex keys trust to the common
  repository root, so Ouija derives `<trust-root>` from `git rev-parse
  --git-common-dir` and uses the parent when that common dir ends in `.git`.

Resuming continues the latest thread in the cwd via `codex resume --last`, or a
specific one via `codex resume <session-id>`.

## Model routes and provider selection

By default, Ouija does **not** pick a model or provider for Codex. If a session
does not set `--model`, Codex uses its own default configuration, usually
`~/.codex/config.toml`.

For provider-specific setups, configure a Codex model route. The route maps the
user-facing Ouija model alias to the actual Codex model and optional Codex home:

```bash
ouija config set-codex-model-route gemini \
  --model gemini-2.5-pro \
  --codex-home ~/.cache/codex-gemini

ouija spawn-session worker --backend codex-cli --model gemini \
  --no-parent-session --idle-policy keep-open
```

Codex requires a Responses-compatible endpoint. Google's OpenAI-compatible
Gemini endpoint currently documents Chat Completions, so use a local sidecar
that exposes `/v1/responses` and routes to Gemini. One verified option is a
self-hosted LiteLLM proxy:

```yaml
model_list:
  - model_name: gemini-2.5-pro
    litellm_params:
      model: gemini/gemini-2.5-pro
      api_key: os.environ/GEMINI_API_KEY
litellm_settings:
  drop_params: true
```

Then create the Codex home's `config.toml`:

```toml
model = "gemini-2.5-pro"
model_provider = "local-litellm-gemini"

[model_providers.local-litellm-gemini]
name = "Local LiteLLM Gemini"
base_url = "http://127.0.0.1:4000/v1"
env_key = "LITELLM_API_KEY"
wire_api = "responses"
```

The Gemini API key stays in the sidecar environment (`GEMINI_API_KEY`). Codex
only sees the local proxy key named by `env_key`; for a private localhost proxy
that can be a dummy value such as `sk-local`. Ouija stores only the route alias,
actual Codex model, and Codex home path. To remove the route:

```bash
ouija config remove-codex-model-route gemini
```

There is also an advanced global `codex_home` setting for deployments that want
all Ouija-launched Codex sessions to use the same alternate home. Do not use it
for selective Gemini routing; use a model route instead.

There is **no Codex `--effort` flag**. Ouija maps its existing `effort` setting
to Codex config instead:

```bash
codex -c 'model_reasoning_effort="low"'
```

The Codex manual documents `model_reasoning_effort` levels including `ultra`,
`max`, `xhigh`, `high`, `medium`, `low`, `minimal`, and `none`; some lower or
higher levels are model-dependent.

## Worktrees are Ouija-managed, not Codex-managed

Codex CLI has no `--worktree` flag. Ouija (or Hub) sets up the worktree/branch
before launch and starts Codex inside it with `cd`. Codex's own app-managed
worktrees (under `$CODEX_HOME/worktrees`, detached HEAD by default) are a separate
feature and are **not** used by the Ouija backend. Use Ouija's `--worktree` /
`--branch` options on `spawn-session` as usual, along with explicit lifecycle
flags such as `--parent-session hub --idle-policy ask-parent-when-done` or
`--no-parent-session --idle-policy close-when-done`.

## Lifecycle: turn-scoped Stop, liveness-based cleanup

Codex fires a `Stop` hook after **every** assistant turn and has **no**
`SessionEnd` event. Consequences:

- The Codex `Stop` hook only performs turn bookkeeping (pending-reply / idle
  checks) and returns `{"continue":true}`. It **never unregisters** the session.
- Session cleanup is driven by **pane/process liveness**, not a hook. Ouija's
  `pane_alive` process-tree walk detects when the pane or the `codex` process
  (a descendant of the `node`/npx wrapper) is gone and reaps the session.

## Mesh instructions and sender identity

Codex learns the mesh two complementary ways:

1. **Installed skill.** `~/.codex/skills/ouija/SKILL.md` (the same skill Claude
   Code and OpenCode use) is installed into Codex's skill-discovery path, so Codex
   can activate it on incoming `<msg from="…">` tags. Codex tool shells may run
   without `TMUX_PANE` or `OUIJA_SESSION_ID`, so the Codex adapter reads its
   native `CODEX_THREAD_ID` and presents it through Ouija's generic backend
   identity contract. The daemon accepts it only when the backend name and
   opaque session id match the values recorded by the SessionStart hook.

2. **Dynamic `SessionStart` `additionalContext`.** The static skill cannot know a
   session's *live* public Ouija id, so on session start Ouija still returns short
   mesh-CLI instructions that the Codex `SessionStart` hook surfaces as
   `additionalContext`, with the concrete id wired in:

   ```
   ouija ls
   ouija ask <target> "question" --from <your-public-id>
   ouija tell <target> "note" --from <your-public-id>
   ouija reply <target> <msg-id> "answer" --from <your-public-id>
   ```

   For generated or multi-line text, use `--stdin` or `--message-file` instead
   of putting the message body in shell quotes.

   `--from <your-public-id>` is included because Codex's bash tool cannot be relied
   on to carry `TMUX_PANE` for sender resolution — but since `OUIJA_SESSION_ID` is
   inherited, commands without `--from` also resolve correctly via `ouija whoami`.
   `ouija ask` is not a synchronous wait operation: it returns after delivery, and
   the eventual reply is pushed into the asking session later as a `<msg ... re>`
   message. A Codex session with no other work should end its turn after asking,
   not poll the message log or pane output.

## Hook trust

Codex reviews hooks before running them. On a normal install you may be prompted
to trust `~/.codex/hooks.json` the first time a Codex session starts — approve it
so the register/activity/stop hooks can run. Automated tests bypass this with
`--dangerously-bypass-hook-trust`; do not use that flag for interactive use.

## Availability detection

`is_available()` uses a timeout so a slow or hanging `codex --version` (Codex is
often installed via an npx/node wrapper that can stall) cannot block daemon
startup or per-session backend detection.
