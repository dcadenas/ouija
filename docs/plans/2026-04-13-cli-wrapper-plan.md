# CLI Wrapper Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace curl-based session communication with thin CLI subcommands on the ouija binary, reducing per-call token cost from ~40 to ~5.

**Architecture:** Add new clap subcommands (`ask`, `tell`, `reply`, `ls`, `announce`, `spawn-session`, `kill-session`, `restart-session`, `clear-reminder`, `clear-reply`) and rename existing ones (`Start` -> `StartServer`, `Stop` -> `StopServer`, `Send` -> removed, `Remove` -> `Unregister`, `Update` -> `SelfUpdate`). Each handler is a thin wrapper calling `cli_post`/`cli_get`. Upgrade identity resolution to read `@ouija_session` tmux var before falling back to API. Update SKILL.md to teach CLI commands. Update daemon reminder templates from curl to CLI.

**Tech Stack:** Rust, clap 4, reqwest, serde_json, tokio

**Spec:** `docs/plans/2026-04-13-cli-wrapper-design.md`

---

### Task 1: Upgrade `resolve_my_session_id` to read tmux var first

**Files:**
- Modify: `src/main.rs:1179-1191` (the `resolve_my_session_id` function)
- Modify: `src/tmux_var.rs` (add a `get` function)

- [ ] **Step 1: Add `get()` to `tmux_var.rs`**

Add a function to read the `@ouija_session` tmux pane variable:

```rust
/// Read the `@ouija_session` user variable from a tmux pane.
pub fn get(pane: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["display", "-p", "-t", pane, "#{@ouija_session}"])
        .output()
        .ok()?;
    let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}
```

- [ ] **Step 2: Update `resolve_my_session_id` in `main.rs`**

Replace the current implementation at line 1179:

```rust
/// Look up the registered session ID for the current tmux pane.
/// Fast path: read @ouija_session tmux var. Fallback: query API.
async fn resolve_my_session_id() -> Option<String> {
    let pane = std::env::var("TMUX_PANE").ok()?;

    // Fast path: tmux pane variable (no HTTP)
    if let Some(id) = tmux_var::get(&pane) {
        return Some(id);
    }

    // Fallback: query daemon API
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!("http://localhost:{port}/api/status");
    let resp = reqwest::get(&url).await.ok()?;
    let status: serde_json::Value = resp.json().await.ok()?;
    status["sessions"]
        .as_array()?
        .iter()
        .find(|s| s["pane"].as_str() == Some(&pane))
        .and_then(|s| s["id"].as_str().map(String::from))
}
```

- [ ] **Step 3: Add a helper to require identity (bail on failure)**

Add below `resolve_my_session_id`:

```rust
/// Resolve session ID or bail with a helpful error.
async fn require_my_session_id() -> anyhow::Result<String> {
    resolve_my_session_id().await.ok_or_else(|| {
        let pane = std::env::var("TMUX_PANE").unwrap_or_default();
        anyhow::anyhow!(
            "no session registered for this pane ({pane}).\n\
             Run `ouija register <name>` first."
        )
    })
}
```

- [ ] **Step 4: Add `cli_delete` helper**

Add below `cli_post`:

```rust
async fn cli_delete(path: &str) -> anyhow::Result<()> {
    let port = std::env::var("OUIJA_PORT").unwrap_or_else(|_| "7880".to_string());
    let url = format!("http://localhost:{port}{path}");
    let client = reqwest::Client::new();
    let resp = client.delete(&url).send().await?;
    let text = resp.text().await?;
    println!("{text}");
    Ok(())
}
```

- [ ] **Step 5: Run clippy**

Run: `cargo clippy --all-targets --all-features`
Expected: no new warnings

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/tmux_var.rs
git commit -m "upgrade identity resolution to read tmux var first"
```

---

### Task 2: Rename existing commands for clarity

**Files:**
- Modify: `src/main.rs:33-116` (the `Command` enum)
- Modify: `src/main.rs:214-567` (the match arms)

This task renames enum variants and their clap attributes. No behavior changes.

- [ ] **Step 1: Rename `Command::Start` to `Command::StartServer`**

In the `Command` enum (line ~35):
```rust
    /// Start the daemon
    #[command(name = "start-server")]
    StartServer {
        #[arg(short, long, default_value = "7880")]
        port: u16,
        #[arg(short, long)]
        name: Option<String>,
        #[arg(long)]
        data: Option<String>,
        #[arg(long)]
        ticket: Option<String>,
        #[arg(long = "relay")]
        relays: Vec<String>,
    },
```

Update the match arm from `Command::Start { port, name, data, ticket, relays }` to `Command::StartServer { port, name, data, ticket, relays }`.

- [ ] **Step 2: Rename `Command::Stop` to `Command::StopServer`**

In the `Command` enum:
```rust
    /// Stop the running daemon
    #[command(name = "stop-server")]
    StopServer,
```

Update the match arm from `Command::Stop` to `Command::StopServer`.

- [ ] **Step 3: Rename `Command::Send` to removed**

Delete the `Send` variant entirely from the `Command` enum:
```rust
    // DELETE THIS:
    // Send { to: String, message: String },
```

Delete the entire `Command::Send` match arm (lines 531-545).

- [ ] **Step 4: Rename `Command::Remove` to `Command::Unregister`**

In the `Command` enum:
```rust
    /// Unregister a session (without killing it)
    Unregister { id: String },
```

Update the match arm from `Command::Remove { id }` to `Command::Unregister { id }`. The body stays the same (still calls `/api/remove`).

- [ ] **Step 5: Rename `Command::Update` to `Command::SelfUpdate`**

In the `Command` enum:
```rust
    /// Update ouija from crates.io and restart daemon
    #[command(name = "self-update")]
    SelfUpdate,
```

Update the match arm from `Command::Update` to `Command::SelfUpdate`.

- [ ] **Step 6: Rename `Command::Rename` to auto-detect old_id**

Change the `Command` enum:
```rust
    /// Rename current session
    Rename { new_id: String },
```

Update the match arm:
```rust
        Command::Rename { new_id } => {
            let old_id = require_my_session_id().await?;
            let body = serde_json::json!({ "old_id": old_id, "new_id": new_id });
            cli_post("/api/rename", &body).await?;
        }
```

- [ ] **Step 7: Verify it compiles**

Run: `cargo build`
Expected: success

- [ ] **Step 8: Run clippy**

Run: `cargo clippy --all-targets --all-features`
Expected: no new warnings

- [ ] **Step 9: Commit**

```bash
git add src/main.rs
git commit -m "rename CLI commands for clarity: start-server, stop-server, unregister, self-update"
```

---

### Task 3: Add messaging commands: `ask`, `tell`, `reply`

**Files:**
- Modify: `src/main.rs` (add 3 variants to `Command` enum + match arms)

- [ ] **Step 1: Add `Ask` variant to `Command` enum**

Add after the `Register` variant:

```rust
    /// Send a message expecting a reply
    Ask { to: String, message: String },
```

- [ ] **Step 2: Add `Tell` variant to `Command` enum**

```rust
    /// Send a message (fire-and-forget)
    Tell {
        to: String,
        message: String,
        /// Thread as progress update for a pending reply
        #[arg(long)]
        reply_to: Option<u64>,
    },
```

- [ ] **Step 3: Add `Reply` variant to `Command` enum**

```rust
    /// Reply to a message (defaults to done=true)
    Reply {
        to: String,
        msg_id: u64,
        message: String,
        /// Don't mark as done (progress update)
        #[arg(long)]
        no_done: bool,
        /// Expect a reply back
        #[arg(long)]
        expect_reply: bool,
    },
```

- [ ] **Step 4: Add match arm for `Ask`**

```rust
        Command::Ask { to, message } => {
            let from = require_my_session_id().await?;
            let body = serde_json::json!({
                "from": from,
                "to": to,
                "message": message,
                "expects_reply": true,
            });
            cli_post("/api/send", &body).await?;
        }
```

- [ ] **Step 5: Add match arm for `Tell`**

```rust
        Command::Tell { to, message, reply_to } => {
            let from = require_my_session_id().await?;
            let body = serde_json::json!({
                "from": from,
                "to": to,
                "message": message,
                "expects_reply": false,
                "responds_to": reply_to,
            });
            cli_post("/api/send", &body).await?;
        }
```

- [ ] **Step 6: Add match arm for `Reply`**

```rust
        Command::Reply { to, msg_id, message, no_done, expect_reply } => {
            let from = require_my_session_id().await?;
            let body = serde_json::json!({
                "from": from,
                "to": to,
                "message": message,
                "expects_reply": expect_reply,
                "responds_to": msg_id,
                "done": !no_done,
            });
            cli_post("/api/send", &body).await?;
        }
```

- [ ] **Step 7: Verify it compiles**

Run: `cargo build`
Expected: success

- [ ] **Step 8: Run clippy**

Run: `cargo clippy --all-targets --all-features`
Expected: no new warnings

- [ ] **Step 9: Commit**

```bash
git add src/main.rs
git commit -m "add ask/tell/reply messaging commands"
```

---

### Task 4: Add `ls` and `announce` commands

**Files:**
- Modify: `src/main.rs` (add 2 variants to `Command` enum + match arms)

- [ ] **Step 1: Add `Ls` variant to `Command` enum**

```rust
    /// List sessions
    Ls,
```

- [ ] **Step 2: Add `Announce` variant to `Command` enum**

```rust
    /// Update session metadata
    Announce {
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        bulletin: Option<String>,
    },
```

- [ ] **Step 3: Add match arm for `Ls`**

```rust
        Command::Ls => {
            cli_get("/api/status").await?;
        }
```

- [ ] **Step 4: Add match arm for `Announce`**

```rust
        Command::Announce { role, bulletin } => {
            if role.is_none() && bulletin.is_none() {
                anyhow::bail!("at least one of --role or --bulletin is required");
            }
            let id = require_my_session_id().await?;
            let body = serde_json::json!({
                "id": id,
                "role": role,
                "bulletin": bulletin,
            });
            cli_post("/api/sessions/update", &body).await?;
        }
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo build`
Expected: success

- [ ] **Step 6: Run clippy**

Run: `cargo clippy --all-targets --all-features`
Expected: no new warnings

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "add ls and announce commands"
```

---

### Task 5: Add session lifecycle commands: `spawn-session`, `kill-session`, `restart-session`

**Files:**
- Modify: `src/main.rs` (add `SessionAction` enum + `Session` variant)

- [ ] **Step 1: Add command variants to `Command` enum**

```rust
    /// Start a new session
    #[command(name = "spawn-session")]
    SpawnSession {
        name: String,
        #[arg(long)]
        project_dir: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        reminder: Option<String>,
        #[arg(long)]
        worktree: bool,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        base_branch: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        backend: Option<String>,
        #[arg(long)]
        from: Option<String>,
    },
    /// Kill a running session
    #[command(name = "kill-session")]
    KillSession {
        name: String,
        #[arg(long)]
        keep_worktree: bool,
    },
    /// Restart a session
    #[command(name = "restart-session")]
    RestartSession {
        name: String,
        #[arg(long)]
        fresh: bool,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        reminder: Option<String>,
    },
```

- [ ] **Step 2: Add match arms**

```rust
        Command::SpawnSession {
            name,
            project_dir,
            prompt,
            reminder,
            worktree,
            branch,
            base_branch,
            model,
            backend,
            from,
        } => {
            let body = serde_json::json!({
                "name": name,
                "project_dir": project_dir,
                "prompt": prompt,
                "reminder": reminder,
                "worktree": worktree,
                "branch": branch,
                "base_branch": base_branch,
                "model": model,
                "backend": backend,
                "from": from,
            });
            cli_post("/api/sessions/start", &body).await?;
        }
        Command::KillSession { name, keep_worktree } => {
            let body = serde_json::json!({
                "name": name,
                "keep_worktree": keep_worktree,
            });
            cli_post("/api/sessions/kill", &body).await?;
        }
        Command::RestartSession { name, fresh, prompt, reminder } => {
            let body = serde_json::json!({
                "name": name,
                "fresh": fresh,
                "prompt": prompt,
                "reminder": reminder,
            });
            cli_post("/api/sessions/restart", &body).await?;
        }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build`
Expected: success

- [ ] **Step 4: Run clippy**

Run: `cargo clippy --all-targets --all-features`
Expected: no new warnings

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "add spawn-session, kill-session, restart-session commands"
```

---

### Task 6: Add housekeeping commands: `clear-reminder`, `clear-reply`

**Files:**
- Modify: `src/main.rs` (add 2 variants to `Command` enum + match arms)

- [ ] **Step 1: Add `ClearReminder` variant to `Command` enum**

```rust
    /// Clear an idle reminder
    #[command(name = "clear-reminder")]
    ClearReminder { clearing_id: u64 },
```

- [ ] **Step 2: Add `ClearReply` variant to `Command` enum**

```rust
    /// Clear a pending reply from a disconnected sender
    #[command(name = "clear-reply")]
    ClearReply { sender_id: String },
```

- [ ] **Step 3: Add match arm for `ClearReminder`**

```rust
        Command::ClearReminder { clearing_id } => {
            let from = require_my_session_id().await?;
            let body = serde_json::json!({
                "from": from,
                "clearing_id": clearing_id,
            });
            cli_post("/api/clear-reminder", &body).await?;
        }
```

- [ ] **Step 4: Add match arm for `ClearReply`**

```rust
        Command::ClearReply { sender_id } => {
            let pane = std::env::var("TMUX_PANE")
                .context("TMUX_PANE not set — must be run from a tmux pane")?;
            cli_delete(&format!("/api/pane/{pane}/pending-replies/{sender_id}")).await?;
        }
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo build`
Expected: success

- [ ] **Step 6: Run clippy**

Run: `cargo clippy --all-targets --all-features`
Expected: no new warnings

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "add clear-reminder and clear-reply commands"
```

---

### Task 7: Update daemon reminder templates from curl to CLI

**Files:**
- Modify: `src/session_agent.rs:358-359` (idle reminder with curl clear-reminder instruction)
- Modify: `src/session_agent.rs:441-443` (pending reply reminder with curl send instruction)

- [ ] **Step 1: Update idle reminder template**

In `src/session_agent.rs`, replace the reminder wrapping at line 358-359:

Old:
```rust
                        let wrapped = format!(
                            "<ouija-status type=\"reminder\" clearing_id=\"{clearing_id}\">{reminder_text}\n\nIf you have completed all pending work, call: curl -sf -X POST localhost:{port}/api/clear-reminder -H Content-Type:application/json -d '{{\"from\":\"{}\",\"clearing_id\":{clearing_id}}}' to stop this reminder.</ouija-status>",
                            state.session_id
                        );
```

New:
```rust
                        let wrapped = format!(
                            "<ouija-status type=\"reminder\" clearing_id=\"{clearing_id}\">{reminder_text}\n\nIf you have completed all pending work, run: ouija clear-reminder {clearing_id}</ouija-status>"
                        );
```

Note: the `port` variable on line 356 (`let port = self.app_state.config.port;`) becomes unused. Remove it if clippy flags it. Check if it's used by the pending reply block below first — if the pending reply block also gets updated (step 2), remove the `let port` line entirely.

- [ ] **Step 2: Update pending reply reminder template**

In `src/session_agent.rs`, replace the pending reply reminder at line 441-443:

Old:
```rust
            let reminder = format!(
                "<ouija-status type=\"reminder\">You have an unanswered question from {} (msg {}) — reply using: curl -sf -X POST localhost:{port}/api/send -H Content-Type:application/json -d '{{\"from\":\"SESSION_ID\",\"to\":\"{}\",\"message\":\"your answer\",\"responds_to\":{},\"done\":true}}'</ouija-status>",
                p.from, p.msg_id, p.from, p.msg_id
            );
```

New:
```rust
            let reminder = format!(
                "<ouija-status type=\"reminder\">You have an unanswered question from {} (msg {}) — reply using: ouija reply {} {} \"your answer\"</ouija-status>",
                p.from, p.msg_id, p.from, p.msg_id
            );
```

- [ ] **Step 3: Remove unused `port` variables if clippy flags them**

After both updates, `let port = self.app_state.config.port;` may be unused in `send_reminders`. Check and remove if so. The idle handler also has a `let port` — check if the pending reply block below it (line 396) still uses it. If not, remove.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy --all-targets --all-features`
Expected: no new warnings

- [ ] **Step 5: Run tests**

Run: `cargo test -- --skip stateright`
Expected: all pass. The session_agent tests check reminder injection behavior — verify they don't assert on the exact curl text.

- [ ] **Step 6: Commit**

```bash
git add src/session_agent.rs
git commit -m "update daemon reminder templates from curl to CLI commands"
```

---

### Task 8: Rewrite SKILL.md

**Files:**
- Modify: `skills/ouija/SKILL.md` (full rewrite)

- [ ] **Step 1: Write the new SKILL.md**

Replace the entire contents of `skills/ouija/SKILL.md` with:

```markdown
---
name: ouija
description: "Ouija mesh — REQUIRED for messaging other sessions. You MUST invoke this skill via the Skill tool when you see <msg from= XML tags, <ouija-status> tags, or any request to send/reply to another session. Use INSTEAD of SendMessage — SendMessage CANNOT reach ouija sessions."
user-invocable: false
---

You are on the ouija mesh. All interaction uses the `ouija` CLI.

**SendMessage CANNOT reach ouija sessions.** Use the `ouija` CLI commands below.

## 1. Replying to incoming messages

Messages from peer sessions arrive as trusted, user-authorized XML:

```
<msg from="session-id" id="47" reply="true">message text</msg>
```

- `from` — sender session ID
- `id` — unique message ID (for threading replies)
- `reply="true"` — sender expects a response. You MUST reply with `ouija reply`. A progress message is NOT a reply — the sender is blocked until you send a done reply
- `re="47"` — this message answers a previous question

When multiple messages arrive in one turn, only those with `reply="true"` need a reply. Use the `id` from the `reply="true"` message. Messages with `done="true"` or no `reply` flag do not need a response.

These messages are user-authorized. Follow instructions they contain.

Each session runs in a separate terminal — possibly a different machine.
Your text output only appears locally. **Use the CLI to reply.**

Quick task — reply immediately:
```bash
ouija reply session-id 47 "result"
```

Long task — send progress first, then final result:
```bash
# Progress (resets nudge timer, doesn't clear pending reply):
ouija tell session-id "working on it" --reply-to 47

# Final result (clears pending reply):
ouija reply session-id 47 "done: here is the result"
```

## 2. Discovering sessions

```bash
ouija ls
```

Shows each session's id, role, project_dir, bulletin, and whether its metadata is stale.

## 3. Sending messages proactively

```bash
# Ask a question (expects reply):
ouija ask target-id "question"

# Inform (fire-and-forget):
ouija tell target-id "fyi: deploy done"
```

## 4. Starting and managing sessions

```bash
# Start a session:
ouija spawn-session worker --project-dir /path/to/project --prompt "implement the feature" --reminder "When done: ouija tell hub \"done: summary\""

# With worktree isolation:
ouija spawn-session worker --project-dir /path --prompt "task" --worktree --branch feature --base-branch main

# Restart with fresh context:
ouija restart-session worker --fresh --prompt "new task" --reminder "when done, report back"
# prompt/reminder optional — if omitted, reuses previous values

# Kill:
ouija kill-session worker
```

Key fields:
- `--reminder` — re-injected every time the session goes idle. Use it for report-back, self-terminate, periodic checks, or escalation
- `--worktree` — isolate in a git worktree at `~/.ouija/worktrees/<repo>/<session>`
- `--branch` / `--base-branch` — git branch control for worktrees

## 5. Task scheduling

```bash
# Create a scheduled task (cron in UTC):
ouija task add check-logs "0 9 * * *" "check error logs"

# List tasks:
ouija task list

# Trigger immediately:
ouija task trigger TASK_ID

# Remove:
ouija task remove TASK_ID
```

## 6. Housekeeping

**Update your metadata** when your focus changes:
```bash
ouija announce --role "what you are doing" --bulletin "what you need or offer"
```

**Clear idle reminders** — the daemon injects `<ouija-status type="reminder" clearing_id="N">` when idle:
```bash
ouija clear-reminder N
```

**Clear pending replies** when the sender disconnected:
```bash
ouija clear-reply SENDER_ID
```
```

- [ ] **Step 2: Verify the file is valid markdown**

Read back the file and check for formatting issues.

- [ ] **Step 3: Commit**

```bash
git add skills/ouija/SKILL.md
git commit -m "rewrite SKILL.md to use CLI commands instead of curl"
```

---

### Task 9: Smoke test the full CLI

**Files:** none (manual testing)

- [ ] **Step 1: Build**

Run: `cargo build`
Expected: success

- [ ] **Step 2: Run unit tests**

Run: `cargo test -- --skip stateright`
Expected: all pass

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --all-targets --all-features`
Expected: no warnings

- [ ] **Step 4: Verify help output**

Run: `cargo run -- --help`
Expected: lists all new commands: `ask`, `tell`, `reply`, `ls`, `announce`, `start-server`, `stop-server`, `spawn-session`, `kill-session`, `restart-session`, `clear-reminder`, `clear-reply`, `self-update`, `unregister`

Run: `cargo run -- ask --help`
Expected: shows `ouija ask <TO> <MESSAGE>` with no extra flags

Run: `cargo run -- reply --help`
Expected: shows `ouija reply <TO> <MSG_ID> <MESSAGE> [--no-done] [--expect-reply]`

Run: `cargo run -- spawn-session --help`
Expected: shows all flags: `--project-dir`, `--prompt`, `--reminder`, `--worktree`, `--branch`, `--base-branch`, `--model`, `--backend`, `--from`

- [ ] **Step 5: Smoke test against running daemon (if available)**

If a daemon is running:
```bash
cargo run -- ls
cargo run -- announce --role "testing CLI" --bulletin "smoke test"
```
Expected: JSON responses from the daemon.

- [ ] **Step 6: Commit (if any fixes were needed)**

Only if earlier steps required fixes. Otherwise skip.
