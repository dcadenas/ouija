use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// Known app commands that support bracketed-paste injection.
const KNOWN_APPS: &[&str] = &["claude"];

/// Lines of scrollback to capture for pane content checks.
const CAPTURE_SCROLL_LINES: &str = "-20";
/// Max message prefix length used for injection verification.
const VERIFY_NEEDLE_LEN: usize = 60;
/// Delay for vim mode keypress detection.
const VIM_DETECT_MS: u64 = 100;
/// Delay for vim backspace to settle.
const VIM_BACKSPACE_MS: u64 = 50;
/// Delay after paste before submitting Enter (React/Ink processing time).
const PASTE_SETTLE_MS: u64 = 300;
/// Delay before verification capture.
const VERIFY_DELAY_MS: u64 = 100;

#[derive(Debug, Clone)]
pub struct TmuxPane {
    pub pane_id: String,
    pub session_name: String,
}

pub fn find_claude_panes() -> anyhow::Result<Vec<TmuxPane>> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{session_name}\t#{pane_current_command}",
        ])
        .output()
        .context("failed to run tmux")?;

    if !output.status.success() {
        bail!("tmux not running or not available");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let panes = stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 3 && parts[2] == "claude" {
                Some(TmuxPane {
                    pane_id: parts[0].to_string(),
                    session_name: parts[1].to_string(),
                })
            } else {
                None
            }
        })
        .collect();

    Ok(panes)
}

/// Check if a tmux pane exists and has `claude` in its process tree.
pub fn pane_alive(pane_id: &str) -> bool {
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

    has_claude_descendant(pane_pid)
}

/// Walk the process tree rooted at `root` looking for a process named `claude`.
fn has_claude_descendant(root: u32) -> bool {
    let output = match Command::new("ps")
        .args(["-eo", "pid,ppid,comm"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
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

    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if names.get(&pid).is_some_and(|n| n == "claude") {
            return true;
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids);
        }
    }

    false
}

/// Log a warning if the pane is not running a known app (e.g. `claude`).
///
/// This is purely informational — injection proceeds regardless, since
/// messages queued in the terminal input buffer are picked up when the
/// app's turn ends.
fn check_known_app(pane: &str) {
    let output = Command::new("tmux")
        .args(["display-message", "-t", pane, "-p", "#{pane_current_command}"])
        .output();

    let cmd = match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => {
            tracing::warn!(pane, "could not detect pane command");
            return;
        }
    };

    if !KNOWN_APPS.iter().any(|&app| cmd == app) {
        tracing::warn!(pane, app = %cmd, "pane is not running a known app");
    }
}

/// Ensure the pane is in INSERT mode for vim-enabled sessions.
///
/// Sends `i` and checks whether it appeared as text on the prompt.
/// If it did, we were already in INSERT mode — backspace removes it.
/// If it didn't, the `i` entered INSERT mode from NORMAL mode.
/// Either way, the pane is in INSERT mode and ready for text.
fn ensure_insert_mode(pane: &str) -> anyhow::Result<()> {
    let before = prompt_text(pane)?;

    let _ = Command::new("tmux")
        .args(["send-keys", "-t", pane, "i"])
        .status();
    thread::sleep(Duration::from_millis(VIM_DETECT_MS));

    let after = prompt_text(pane)?;

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

/// Extract the text after ❯ on the last prompt line.
fn prompt_text(pane: &str) -> anyhow::Result<String> {
    let content = capture_pane(pane)?;
    let text = content
        .lines()
        .rev()
        .find(|l| l.contains('\u{276F}'))
        .and_then(|line| line.split('\u{276F}').nth(1))
        .unwrap_or("")
        .to_string();
    Ok(text)
}

pub fn inject(pane: &str, message: &str, vim_mode: bool) -> anyhow::Result<()> {
    let t0 = Instant::now();
    check_known_app(pane);
    let t1 = Instant::now();

    if vim_mode {
        ensure_insert_mode(pane)?;
    }
    let t2 = Instant::now();

    inject_text(pane, message)?;
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
/// Wraps the text in bracketed paste sequences (`ESC[200~...ESC[201~`)
/// inside the buffer, then uses `paste-buffer` (which adds its own outer
/// bracket layer). This is necessary because Claude Code's TUI autocomplete
/// intercepts individual keystrokes from `send-keys -l`, silently swallowing
/// them. The explicit inner brackets ensure the TUI receives the text as a
/// paste event and inserts it into the input buffer. After the paste completes,
/// the TUI exits paste mode and `send-keys Enter` submits normally.
///
/// Newlines are replaced with spaces to prevent multiline paste behavior.
fn inject_text(pane: &str, message: &str) -> anyhow::Result<()> {
    let sanitized = message.replace('\n', " ");

    // Wrap in bracketed paste sequences so the TUI treats it as pasted text
    let paste_content = format!("\x1b[200~{sanitized}\x1b[201~");

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
    // Claude Code's React/Ink runtime needs time to handle the bracketed
    // paste; 50ms is too short, 300ms is reliable in testing.
    thread::sleep(Duration::from_millis(PASTE_SETTLE_MS));

    let status = Command::new("tmux")
        .args(["send-keys", "-t", pane, "Enter"])
        .status()
        .context("failed to run tmux send-keys Enter")?;

    if !status.success() {
        tracing::warn!("tmux send-keys Enter failed for pane {pane}");
    }

    Ok(())
}

pub fn format_peer_message(from: &str, message: &str) -> String {
    format!("[from {from}]: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_peer_message_basic() {
        assert_eq!(
            format_peer_message("alice", "hello"),
            "[from alice]: hello"
        );
    }

    #[test]
    fn format_peer_message_empty() {
        assert_eq!(format_peer_message("x", ""), "[from x]: ");
    }
}
