use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// Lines of scrollback to capture for pane content checks.
const CAPTURE_SCROLL_LINES: &str = "-20";
/// Max message prefix length used for injection verification.
const VERIFY_NEEDLE_LEN: usize = 60;
/// Delay for vim mode keypress detection.
const VIM_DETECT_MS: u64 = 100;
/// Delay for vim backspace to settle.
const VIM_BACKSPACE_MS: u64 = 50;
/// Delay before verification capture.
const VERIFY_DELAY_MS: u64 = 100;
/// Max retry attempts for pane injection (pane busy / mid-output).
const MAX_INJECT_RETRIES: u32 = 3;
/// Base delay for exponential backoff between retries (500ms, 1s, 2s).
const RETRY_BASE_MS: u64 = 500;

#[derive(Debug, Clone)]
pub struct TmuxPane {
    pub pane_id: String,
    pub session_name: String,
    pub pane_current_path: Option<String>,
    pub process_name: Option<String>,
}

/// Parsed process tree snapshot for efficient descendant lookups.
struct ProcessTree {
    children: std::collections::HashMap<u32, Vec<u32>>,
    names: std::collections::HashMap<u32, String>,
}

impl ProcessTree {
    /// Take a snapshot of all processes via `ps`.
    fn snapshot() -> Option<Self> {
        let output = Command::new("ps")
            .args(["-eo", "pid,ppid,comm"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut children: std::collections::HashMap<u32, Vec<u32>> =
            std::collections::HashMap::new();
        let mut names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();

        for line in stdout.lines().skip(1) {
            let mut parts = line.split_whitespace();
            let (Some(pid_s), Some(ppid_s), Some(comm)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(pid), Ok(ppid)) = (pid_s.parse::<u32>(), ppid_s.parse::<u32>()) else {
                continue;
            };
            children.entry(ppid).or_default().push(pid);
            names.insert(pid, comm.to_string());
        }

        Some(Self { children, names })
    }

    /// Check if any descendant of `root` matches one of the given `names`.
    ///
    /// Matches against the full comm string, its basename (last path
    /// component), and dot-prefixed variants (e.g. `.opencode`) since some
    /// binaries appear with full paths in `ps` output (notably on macOS when
    /// installed via Homebrew) or with a leading dot when run via npm/node
    /// wrappers.
    fn has_descendant_named(&self, root: u32, names: &[&str]) -> bool {
        self.matching_descendant_name(root, names).is_some()
    }

    fn matching_descendant_name(&self, root: u32, names: &[&str]) -> Option<String> {
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if let Some(name) = self.names.get(&pid)
                && let Some(target) = matching_process_name(name, names)
            {
                return Some(target.to_string());
            }
            if let Some(kids) = self.children.get(&pid) {
                stack.extend(kids);
            }
        }
        None
    }
}

fn matching_process_name<'a>(name: &str, names: &'a [&str]) -> Option<&'a str> {
    let basename = std::path::Path::new(name)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(name);
    names.iter().copied().find(|target| {
        name == *target || basename == *target || basename.strip_prefix('.') == Some(*target)
    })
}

/// Find all tmux panes that have a matching assistant process.
///
/// Checks `pane_current_command` first (fast path), then falls back to
/// walking the process tree for panes where the assistant runs under a shell.
/// The process snapshot is taken once and reused for all panes.
pub fn find_assistant_panes(names: &[&str]) -> anyhow::Result<Vec<TmuxPane>> {
    const SEP: &str = "|||";
    let format = format!(
        "#{{pane_id}}{SEP}#{{session_name}}{SEP}#{{pane_pid}}{SEP}#{{pane_current_command}}{SEP}#{{pane_current_path}}"
    );
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", &format])
        .output()
        .context("failed to run tmux")?;

    if !output.status.success() {
        bail!("tmux not running or not available");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Lazily snapshot the process tree only if needed (some pane isn't a direct match)
    let mut proc_tree: Option<ProcessTree> = None;
    let needs_tree = stdout.lines().any(|line| {
        let parts: Vec<&str> = line.split(SEP).collect();
        parts.len() >= 5 && !names.contains(&parts[3])
    });
    if needs_tree {
        proc_tree = ProcessTree::snapshot();
    }

    let panes = stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split(SEP).collect();
            if parts.len() >= 5 {
                let process_name = matching_process_name(parts[3], names)
                    .map(str::to_string)
                    .or_else(|| {
                        parts[2].parse::<u32>().ok().and_then(|pid| {
                            proc_tree
                                .as_ref()
                                .and_then(|t| t.matching_descendant_name(pid, names))
                        })
                    });
                if let Some(process_name) = process_name {
                    let path = parts[4].trim();
                    return Some(TmuxPane {
                        pane_id: parts[0].to_string(),
                        session_name: parts[1].to_string(),
                        pane_current_path: if path.is_empty() {
                            None
                        } else {
                            Some(path.to_string())
                        },
                        process_name: Some(process_name),
                    });
                }
            }
            None
        })
        .collect();

    Ok(panes)
}

/// Check if a tmux pane exists and has a matching process in its tree.
pub fn pane_alive(pane_id: &str, names: &[&str]) -> bool {
    let output = match Command::new("tmux")
        .args(["display-message", "-t", pane_id, "-p", "#{pane_pid}"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };

    let pane_pid: u32 = match String::from_utf8_lossy(&output.stdout).trim().parse() {
        Ok(pid) => pid,
        Err(_) => return false,
    };

    ProcessTree::snapshot().is_some_and(|t| t.has_descendant_named(pane_pid, names))
}

/// Log a warning if the pane is not running a known app.
///
/// This is purely informational — injection proceeds regardless, since
/// messages queued in the terminal input buffer are picked up when the
/// app's turn ends.
fn check_known_app(pane: &str, names: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-t",
            pane,
            "-p",
            "#{pane_current_command}",
        ])
        .output();

    let cmd = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            tracing::warn!(pane, "could not detect pane command");
            return Ok(());
        }
    };

    if !names
        .iter()
        .any(|&app| cmd == app || cmd.strip_prefix('.') == Some(app))
    {
        tracing::warn!(
            pane,
            cmd,
            "pane is not running a known app — injecting anyway"
        );
    }
    Ok(())
}

/// Ensure the pane is in INSERT mode for vim-enabled sessions.
///
/// Sends `i` and checks whether it appeared as text on the prompt.
/// If it did, we were already in INSERT mode — backspace removes it.
/// If it didn't, the `i` entered INSERT mode from NORMAL mode.
/// Either way, the pane is in INSERT mode and ready for text.
fn ensure_insert_mode(pane: &str, tui_pattern: &str) -> anyhow::Result<()> {
    let before = prompt_text(pane, tui_pattern)?;

    let _ = Command::new("tmux")
        .args(["send-keys", "-t", pane, "i"])
        .status();
    thread::sleep(Duration::from_millis(VIM_DETECT_MS));

    let after = prompt_text(pane, tui_pattern)?;

    if after.len() > before.len() {
        // `i` appeared as text — was already in INSERT mode, remove it
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", pane, "BSpace"])
            .status();
        thread::sleep(Duration::from_millis(VIM_BACKSPACE_MS));
    }
    // Otherwise `i` was consumed as a vim command — now in INSERT mode

    Ok(())
}

/// Extract the text after a prompt pattern on the last prompt line.
fn prompt_text(pane: &str, pattern: &str) -> anyhow::Result<String> {
    let content = capture_pane(pane)?;
    let text = content
        .lines()
        .rev()
        .find(|l| l.contains(pattern))
        .and_then(|line| line.split(pattern).nth(1))
        .unwrap_or("")
        .to_string();
    Ok(text)
}

/// Inject a message into a tmux pane via paste-buffer.
///
/// Optionally enters vim INSERT mode first, then verifies delivery.
///
/// # Errors
///
/// Returns an error if tmux commands fail or the pane does not exist.
pub fn inject(
    pane: &str,
    message: &str,
    vim_mode: bool,
    config: &crate::backend::InjectConfig,
    tui_pattern: Option<&str>,
) -> anyhow::Result<()> {
    let t0 = Instant::now();
    check_known_app(pane, &[])?;
    let t1 = Instant::now();

    if vim_mode {
        if let Some(pattern) = tui_pattern {
            ensure_insert_mode(pane, pattern)?;
        }
    }
    let t2 = Instant::now();

    inject_text(pane, message, config)?;
    let t3 = Instant::now();

    // Best-effort verification (warns on miss, never fails the delivery)
    thread::sleep(Duration::from_millis(VERIFY_DELAY_MS));
    verify_injected(pane, message);
    let t4 = Instant::now();

    tracing::info!(
        pane,
        msg_len = message.len(),
        check_app_ms = t1.duration_since(t0).as_millis() as u64,
        vim_mode_ms = t2.duration_since(t1).as_millis() as u64,
        inject_ms = t3.duration_since(t2).as_millis() as u64,
        verify_ms = t4.duration_since(t3).as_millis() as u64,
        total_ms = t4.duration_since(t0).as_millis() as u64,
        "inject timing"
    );

    Ok(())
}

fn capture_pane(pane: &str) -> anyhow::Result<String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", pane, "-p", "-S", CAPTURE_SCROLL_LINES])
        .output()
        .context("failed to run tmux capture-pane")?;

    if !output.status.success() {
        bail!(
            "tmux capture-pane failed for pane {pane}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Best-effort verification — warns instead of failing.
///
/// When the target is mid-turn, Claude's output can scroll the injected text
/// off the visible capture window before we check. The message was still
/// delivered (paste-buffer/send-keys are reliable), so a missing needle is
/// not a real failure.
fn verify_injected(pane: &str, message: &str) {
    let content = match capture_pane(pane) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("verify capture failed for pane {pane}: {e}");
            return;
        }
    };

    let needle = if message.len() > VERIFY_NEEDLE_LEN {
        &message[..VERIFY_NEEDLE_LEN]
    } else {
        message
    };

    if !content.contains(needle) {
        tracing::warn!(
            pane,
            "inject verification: text not found in visible area (may have scrolled off)"
        );
    }
}

/// Inject message text via `tmux paste-buffer` then submit with Enter.
///
/// When `config.use_inner_bracketed_paste` is true, wraps the text in
/// bracketed paste sequences (`ESC[200~...ESC[201~`) inside the buffer,
/// then uses `paste-buffer` (which adds its own outer bracket layer).
/// This is necessary for TUIs that intercept individual keystrokes from
/// `send-keys -l`, silently swallowing them. The explicit inner brackets
/// ensure the TUI receives the text as a paste event.
///
/// When inner bracketed paste is disabled, the raw text is loaded into
/// the paste buffer without extra escape sequences.
///
/// Newlines are replaced with spaces to prevent multiline paste behavior.
fn inject_text(
    pane: &str,
    message: &str,
    config: &crate::backend::InjectConfig,
) -> anyhow::Result<()> {
    let sanitized = sanitize_injection_text(message);

    let paste_content = if config.use_inner_bracketed_paste {
        // Wrap in bracketed paste sequences so the TUI treats it as pasted text
        format!("\x1b[200~{sanitized}\x1b[201~")
    } else {
        sanitized
    };

    // Load into tmux paste buffer via stdin
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn tmux load-buffer")?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(paste_content.as_bytes())?;
    }

    let status = child.wait().context("tmux load-buffer failed")?;
    if !status.success() {
        bail!("tmux load-buffer failed for pane {pane}");
    }

    // Paste buffer into target pane (tmux adds outer bracket wrapping)
    let status = Command::new("tmux")
        .args(["paste-buffer", "-t", pane])
        .status()
        .context("failed to run tmux paste-buffer")?;

    if !status.success() {
        bail!("tmux paste-buffer failed for pane {pane}");
    }

    // Wait for the TUI to fully process the paste event before submitting.
    thread::sleep(Duration::from_millis(config.paste_settle_ms));

    let status = Command::new("tmux")
        .args(["send-keys", "-t", pane, "Enter"])
        .status()
        .context("failed to run tmux send-keys Enter")?;

    if !status.success() {
        tracing::warn!("tmux send-keys Enter failed for pane {pane}");
    }

    Ok(())
}

fn sanitize_injection_text(message: &str) -> String {
    message
        .replace('\n', " ")
        .replace("\x1b[200~", "")
        .replace("\x1b[201~", "")
        .chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            c if c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c) => None,
            c => Some(c),
        })
        .collect()
}

/// A queued injection request sent to the per-pane background worker.
#[derive(Debug)]
pub struct InjectRequest {
    pub pane: String,
    pub message: String,
    pub vim_mode: bool,
    pub inject_config: crate::backend::InjectConfig,
    pub tui_pattern: Option<String>,
    pub result_tx: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
}

/// Background worker that drains the FIFO queue for a single pane.
///
/// Messages are processed in order. On failure, retries with exponential
/// backoff before reporting the error back to the caller.
pub async fn pane_inject_loop(mut rx: tokio::sync::mpsc::UnboundedReceiver<InjectRequest>) {
    while let Some(req) = rx.recv().await {
        let mut attempts = 0u32;
        let result = loop {
            let pane = req.pane.clone();
            let message = req.message.clone();
            let vim_mode = req.vim_mode;
            let config = crate::backend::InjectConfig {
                paste_settle_ms: req.inject_config.paste_settle_ms,
                use_inner_bracketed_paste: req.inject_config.use_inner_bracketed_paste,
                startup_inject_delay_secs: req.inject_config.startup_inject_delay_secs,
            };
            let tui_pattern = req.tui_pattern.clone();
            match tokio::task::spawn_blocking(move || {
                inject(&pane, &message, vim_mode, &config, tui_pattern.as_deref())
            })
            .await
            {
                Ok(Ok(())) => break Ok(()),
                Ok(Err(e)) => {
                    attempts += 1;
                    if attempts >= MAX_INJECT_RETRIES {
                        break Err(e);
                    }
                    let delay = RETRY_BASE_MS * 2u64.pow(attempts - 1);
                    tracing::warn!(
                        pane = %req.pane,
                        attempt = attempts,
                        retry_ms = delay,
                        "inject failed, retrying: {e}"
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                Err(e) => break Err(anyhow::anyhow!("spawn_blocking join error: {e}")),
            }
        };
        let _ = req.result_tx.send(result);
    }
}

#[derive(Debug)]
pub(crate) enum SessionDeliveryPlan {
    Http(crate::daemon_protocol::HttpDeliverySnapshot),
    RawTmux {
        inject_config: crate::backend::InjectConfig,
        tui_pattern: Option<String>,
    },
    Unavailable(String),
}

pub(crate) async fn session_delivery_plan(
    state: &crate::state::AppState,
    session_id: &str,
    pane: &str,
) -> SessionDeliveryPlan {
    let Some((metadata, registered_pane)) = ({
        let proto = state.protocol.read().await;
        proto
            .sessions
            .get(session_id)
            .map(|s| (s.metadata.clone(), s.pane.clone()))
    }) else {
        return SessionDeliveryPlan::Unavailable(format!(
            "session '{session_id}' is not registered"
        ));
    };

    let backend = metadata
        .backend
        .as_deref()
        .and_then(|name| state.backends.get(name))
        .unwrap_or_else(|| state.backends.default());

    match backend.delivery_mode() {
        crate::backend::DeliveryMode::TuiInjection => SessionDeliveryPlan::RawTmux {
            inject_config: backend.inject_config(),
            tui_pattern: backend.tui_ready_pattern().map(String::from),
        },
        crate::backend::DeliveryMode::HttpApi { .. } => {
            if let Some(snapshot) = metadata.http_delivery_snapshot() {
                return SessionDeliveryPlan::Http(snapshot);
            }

            if metadata.backend.as_deref() == Some("opencode")
                && registered_pane.as_deref() == Some(pane)
            {
                return SessionDeliveryPlan::RawTmux {
                    inject_config: backend.inject_config(),
                    tui_pattern: backend.tui_ready_pattern().map(String::from),
                };
            }

            SessionDeliveryPlan::Unavailable(format!(
                "session '{session_id}' is not safely deliverable via HTTP and does not own pane '{pane}'"
            ))
        }
    }
}

/// Enqueue a message for injection into a tmux pane.
///
/// Messages are queued in a per-pane FIFO and processed by a background
/// worker. Ordering is preserved and messages are never lost. On injection
/// failure the worker retries with backoff before returning the error.
pub async fn locked_inject(
    state: &crate::state::AppState,
    session_id: &str,
    pane: &str,
    message: &str,
    vim_mode: bool,
) -> anyhow::Result<()> {
    match session_delivery_plan(state, session_id, pane).await {
        SessionDeliveryPlan::Http(delivery) => {
            // locked_inject is the fire-and-forget path used by reminders,
            // session-agent nudges, and similar best-effort senders; log and
            // swallow upstream failures so those callers keep their existing
            // semantics. Callers that need to observe delivery outcomes must
            // call deliver_via_http directly.
            if let Err(decision) = deliver_via_http(
                state,
                &delivery.backend_session_id,
                delivery.project_dir.as_deref(),
                message,
                delivery.model.as_deref(),
                delivery.effort.as_deref(),
            )
            .await
            {
                tracing::warn!(session = %session_id, ?decision, "http delivery failed");
            }
        }
        SessionDeliveryPlan::RawTmux {
            inject_config,
            tui_pattern,
        } => {
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            let req = InjectRequest {
                pane: pane.to_string(),
                message: message.to_string(),
                vim_mode,
                inject_config,
                tui_pattern,
                result_tx,
            };
            state.enqueue_inject(req);
            return result_rx
                .await
                .map_err(|_| anyhow::anyhow!("inject queue closed"))?;
        }
        SessionDeliveryPlan::Unavailable(reason) => anyhow::bail!(reason),
    }

    Ok(())
}

/// Enqueue a message for raw tmux injection regardless of backend delivery mode.
///
/// Use this for explicit pane-targeted delivery where the caller's intent is to
/// drive the visible TUI rather than any backend HTTP session.
pub async fn locked_inject_raw_tmux(
    state: &crate::state::AppState,
    session_id: &str,
    pane: &str,
    message: &str,
    vim_mode: bool,
) -> anyhow::Result<()> {
    if cfg!(test) {
        return Ok(());
    }

    let backend = state.backend_for_session(session_id).await;
    let config = backend.inject_config();
    let tui_pattern = backend.tui_ready_pattern().map(String::from);

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let req = InjectRequest {
        pane: pane.to_string(),
        message: message.to_string(),
        vim_mode,
        inject_config: config,
        tui_pattern,
        result_tx,
    };
    state.enqueue_inject(req);
    result_rx
        .await
        .map_err(|_| anyhow::anyhow!("inject queue closed"))?
}

/// Deliver a message to an opencode session via its HTTP API.
///
/// Uses the `prompt_async` endpoint which returns immediately without waiting
/// for the LLM to finish processing. The message appears as a user message
/// in the session and triggers an assistant turn.
///
/// `model` and `effort` are applied to every request via
/// [`crate::nostr_transport::opencode_prompt_body`]. Opencode's server remembers
/// the last model per session, but the `variant` (effort) is not remembered —
/// so re-sending both on each delivery keeps the session anchored to the
/// operator-requested configuration.
///
/// Returns `Err` on connection failure or any non-2xx response so callers can
/// distinguish delivered from swallowed. Best-effort callers (e.g. the HttpApi
/// branch of `locked_inject`) wrap this in a tracing::warn.
pub(crate) async fn deliver_via_http(
    state: &crate::state::AppState,
    oc_session_id: &str,
    project_dir: Option<&str>,
    message: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(), crate::nostr_transport::PromptAsyncFallbackDecision> {
    let port = state.opencode_serve_port();

    let client = state.http_client.clone();
    let body = crate::nostr_transport::opencode_prompt_body(message, model, effort);

    let async_url = format!("http://127.0.0.1:{port}/session/{oc_session_id}/prompt_async");
    let mut req = client
        .post(&async_url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(10));
    if let Some(dir) = project_dir {
        req = req.header("x-opencode-directory", dir);
    }
    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(error) => {
            return Err(crate::nostr_transport::classify_prompt_async_fallback(
                crate::nostr_transport::PromptAsyncFailure::Request(&error),
            ));
        }
    };

    let status = resp.status();
    if status.is_success() {
        tracing::info!(port, "delivered message via prompt_async");
        Ok(())
    } else {
        let decision = crate::nostr_transport::classify_prompt_async_fallback(
            crate::nostr_transport::PromptAsyncFailure::Status(status),
        );
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!(%status, %text, ?decision, "prompt_async returned non-success");
        Err(decision)
    }
}

/// Rename the tmux window containing a pane and disable automatic-rename.
pub fn rename_window(pane_id: &str, name: &str) {
    if cfg!(test) {
        return;
    }
    let _ = Command::new("tmux")
        .args(["rename-window", "-t", pane_id, name])
        .status();
    let _ = Command::new("tmux")
        .args([
            "set-window-option",
            "-t",
            pane_id,
            "automatic-rename",
            "off",
        ])
        .status();
}

/// Re-enable automatic-rename on the tmux window containing a pane.
pub fn enable_automatic_rename(pane_id: &str) {
    if cfg!(test) {
        return;
    }
    let _ = Command::new("tmux")
        .args(["set-window-option", "-t", pane_id, "automatic-rename", "on"])
        .status();
}

/// Build the `-e KEY=VALUE` argument list handed to `tmux new-window`,
/// `tmux new-session`, and `tmux respawn-pane` when ouija spawns a pane.
///
/// The returned vector is flat and ready to splat into `Command::args(...)`:
/// `["-e", "OUIJA_SESSION_ID=<id>", "-e", "HISTFILE=/dev/null", ...]`.
///
/// `OUIJA_SESSION_ID` is the primary signal the `ouija` CLI uses to resolve
/// the caller's session identity. Exporting it into the spawned shell closes
/// three failure modes seen in the wild:
///   1. The `@ouija_session` tmux pane var is set by a fire-and-forget
///      `spawn_blocking` effect (see `state.rs` `Effect::SetTmuxVar`) that
///      is not awaited; a fast `ouija clear-reminder` call from the newly
///      spawned session can lose this race.
///   2. Opencode bash subshells occasionally do not inherit `TMUX_PANE`.
///   3. Sessions launched outside tmux (future non-tmux backends) have no
///      pane var to read at all.
///
/// `HISTFILE=/dev/null` and `fish_history=` suppress history writes so
/// ouija commands don't pollute the user's shell history.
pub fn pane_env_args(session_id: &str) -> Vec<String> {
    vec![
        "-e".into(),
        format!("OUIJA_SESSION_ID={session_id}"),
        "-e".into(),
        "HISTFILE=/dev/null".into(),
        "-e".into(),
        "fish_history=".into(),
    ]
}

/// Derive a tmux session name from a project directory path.
/// Uses the directory basename with dots replaced by underscores
/// (matching tmux-sessionizer convention).
pub fn tmux_session_name(project_dir: &str) -> String {
    // For ouija-managed worktrees, derive the tmux session from the repo name
    // so worktree sessions join the same tmux session as the main project.
    let basename = if let Some(i) = project_dir.find("/.ouija/worktrees/") {
        let after = &project_dir[i + "/.ouija/worktrees/".len()..];
        // New path: ~/.ouija/worktrees/<repo-slug>/<name> → use repo-slug
        // Legacy path: <repo>/.ouija/worktrees/<name> → use repo basename
        if let Some(slash) = after.find('/') {
            // Has sub-path → repo-slug is the first component
            after[..slash].to_string()
        } else {
            // Legacy: only session name after worktrees/ → use repo basename
            std::path::Path::new(&project_dir[..i])
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| project_dir[..i].to_string())
        }
    } else if let Some(i) = project_dir.find("/.claude/worktrees/") {
        std::path::Path::new(&project_dir[..i])
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| project_dir[..i].to_string())
    } else {
        std::path::Path::new(project_dir)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| project_dir.to_string())
    };
    basename.replace('.', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_env_args_includes_ouija_session_id() {
        let args = pane_env_args("feat/442-chunk-4");
        // Flat -e KEY=VALUE pairs, in order, suitable for splatting into tmux argv
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "OUIJA_SESSION_ID=feat/442-chunk-4"),
            "expected OUIJA_SESSION_ID=<id> in args, got {args:?}"
        );
    }

    #[test]
    fn pane_env_args_preserves_history_suppression() {
        let args = pane_env_args("x");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "HISTFILE=/dev/null"),
            "expected HISTFILE=/dev/null preserved, got {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "fish_history="),
            "expected fish_history= preserved, got {args:?}"
        );
    }

    #[test]
    fn pane_env_args_each_key_prefixed_by_dash_e() {
        // Every VALUE must be immediately preceded by a "-e" flag — no
        // bare values sneaking in that would otherwise be interpreted as
        // the shell-command positional arg to new-window/new-session.
        let args = pane_env_args("abc");
        let mut i = 0;
        while i < args.len() {
            assert_eq!(args[i], "-e", "arg {i} should be -e, got {args:?}");
            assert!(i + 1 < args.len(), "-e at end with no value: {args:?}");
            assert!(
                args[i + 1].contains('='),
                "value without '=': {:?}",
                args[i + 1]
            );
            i += 2;
        }
    }

    #[test]
    fn sanitize_injection_text_strips_escape_and_carriage_return_bytes() {
        let sanitized = sanitize_injection_text("prefix\x1b[201~/quit\rsuffix");

        assert!(!sanitized.contains('\x1b'));
        assert!(!sanitized.contains('\u{9b}'));
        assert!(!sanitized.contains('\r'));
        assert!(!sanitized.contains("[201~"));
        assert_eq!(sanitized, "prefix/quitsuffix");
    }

    #[test]
    fn sanitize_injection_text_neutralizes_other_c0_and_c1_controls() {
        let sanitized = sanitize_injection_text("alpha\0\x07\x08beta\tgamma\u{7f}\u{85}omega");

        assert_eq!(sanitized, "alphabeta gammaomega");
        assert!(
            !sanitized
                .chars()
                .any(|c| { c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c) }),
            "sanitized text still contains C0/C1 controls: {sanitized:?}"
        );
    }

    #[tokio::test]
    async fn session_delivery_plan_uses_raw_tmux_for_weak_opencode_binding() {
        let state = crate::state::AppState::new_for_test();
        state
            .protocol
            .write()
            .await
            .apply(crate::daemon_protocol::Event::Register {
                id: "weak-opencode".into(),
                pane: Some("%42".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("oc-session".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted),
                    ..Default::default()
                },
            });

        let plan = session_delivery_plan(&state, "weak-opencode", "%42").await;

        assert!(
            matches!(plan, SessionDeliveryPlan::RawTmux { .. }),
            "weak/adopted OpenCode sessions must inject into the visible pane, got {plan:?}"
        );
    }

    #[test]
    fn tmux_session_name_basename() {
        assert_eq!(
            tmux_session_name("/home/user/code/divine-mobile"),
            "divine-mobile"
        );
    }

    #[test]
    fn tmux_session_name_dots_replaced() {
        assert_eq!(
            tmux_session_name("/home/user/code/my.project"),
            "my_project"
        );
    }

    #[test]
    fn tmux_session_name_preserves_hyphens_and_underscores() {
        assert_eq!(tmux_session_name("/tmp/some_repo-name"), "some_repo-name");
    }

    #[test]
    fn tmux_session_name_bare_name() {
        assert_eq!(tmux_session_name("ouija"), "ouija");
    }

    #[test]
    fn rename_window_invalid_pane_no_panic() {
        // Should not panic on non-existent pane
        rename_window("%99999", "test");
    }

    #[test]
    fn enable_automatic_rename_invalid_pane_no_panic() {
        // Should not panic on non-existent pane
        enable_automatic_rename("%99999");
    }

    #[test]
    fn has_descendant_named_exact_match() {
        let tree = ProcessTree {
            children: [(1, vec![2]), (2, vec![3])].into_iter().collect(),
            names: [
                (1, "bash".into()),
                (2, "node".into()),
                (3, "opencode".into()),
            ]
            .into_iter()
            .collect(),
        };
        assert!(tree.has_descendant_named(1, &["opencode"]));
    }

    #[test]
    fn has_descendant_named_dot_prefix_match() {
        // opencode via npm shows up as ".opencode" in ps
        let tree = ProcessTree {
            children: [(1, vec![2]), (2, vec![3])].into_iter().collect(),
            names: [
                (1, "bash".into()),
                (2, "node".into()),
                (3, ".opencode".into()),
            ]
            .into_iter()
            .collect(),
        };
        assert!(tree.has_descendant_named(1, &["opencode"]));
    }

    #[test]
    fn has_descendant_named_no_match() {
        let tree = ProcessTree {
            children: [(1, vec![2])].into_iter().collect(),
            names: [(1, "bash".into()), (2, "vim".into())]
                .into_iter()
                .collect(),
        };
        assert!(!tree.has_descendant_named(1, &["opencode", "claude"]));
    }

    #[test]
    fn has_descendant_named_multiple_targets() {
        let tree = ProcessTree {
            children: [(1, vec![2])].into_iter().collect(),
            names: [(1, "bash".into()), (2, "claude".into())]
                .into_iter()
                .collect(),
        };
        assert!(tree.has_descendant_named(1, &["opencode", "claude"]));
    }

    #[test]
    fn has_descendant_named_full_path_basename_match() {
        // On macOS with Homebrew, ps -eo comm returns the full binary path.
        let tree = ProcessTree {
            children: [(1, vec![2]), (2, vec![3])].into_iter().collect(),
            names: [
                (1, "fish".into()),
                (2, "/opt/homebrew/opt/node/bin/node".into()),
                (3, "/opt/homebrew/Cellar/opencode/1.14.30/libexec/lib/node_modules/opencode-ai/node_modules/opencode-darwin-arm64/bin/opencode".into()),
            ]
            .into_iter()
            .collect(),
        };
        assert!(tree.has_descendant_named(1, &["opencode"]));
    }
}
