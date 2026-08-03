use std::collections::BTreeSet;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
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
/// Delay before a deferred re-verification captures the pane.
///
/// The re-check runs from the recipient's own end-of-turn signal, so it gives
/// the TUI a moment to finish its end-of-turn redraw before looking again.
#[cfg_attr(test, allow(dead_code))]
const DEFERRED_VERIFY_DELAY_MS: u64 = 300;
/// Max retry attempts for pane injection (pane busy / mid-output).
const MAX_INJECT_RETRIES: u32 = 3;
/// Base delay for exponential backoff between retries (500ms, 1s, 2s).
const RETRY_BASE_MS: u64 = 500;

/// Upper bound on a single tmux injection attempt.
///
/// A healthy inject measures in the low hundreds of milliseconds (paste settle
/// plus verify), so this is generous headroom rather than a tight deadline.
const INJECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);

static INJECT_BUFFER_SEQ: AtomicU64 = AtomicU64::new(0);

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

    fn descendant_names(&self, root: u32) -> Vec<&str> {
        let mut names = Vec::new();
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if let Some(name) = self.names.get(&pid) {
                names.push(name.as_str());
            }
            if let Some(kids) = self.children.get(&pid) {
                stack.extend(kids);
            }
        }
        names
    }
}

pub(crate) fn matching_process_name<'a>(name: &str, names: &'a [&str]) -> Option<&'a str> {
    let basename = std::path::Path::new(name)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(name);
    names.iter().copied().find(|target| {
        name == *target || basename == *target || basename.strip_prefix('.') == Some(*target)
    })
}

pub(crate) fn matching_backends_for_process_names<'a>(
    process_names: impl IntoIterator<Item = &'a str>,
    backends: &[(String, Vec<String>)],
) -> BTreeSet<String> {
    let process_names = process_names.into_iter().collect::<Vec<_>>();
    backends
        .iter()
        .filter_map(|(backend, names)| {
            let names = names.iter().map(String::as_str).collect::<Vec<_>>();
            process_names
                .iter()
                .any(|process_name| matching_process_name(process_name, &names).is_some())
                .then(|| backend.clone())
        })
        .collect()
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
    let Some(pane_pid) = pane_pid(pane_id) else {
        return false;
    };

    ProcessTree::snapshot().is_some_and(|t| t.has_descendant_named(pane_pid, names))
}

fn pane_pid(pane_id: &str) -> Option<u32> {
    let output = match Command::new("tmux")
        .args(["display-message", "-t", pane_id, "-p", "#{pane_pid}"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return None,
    };

    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

pub(crate) fn backends_in_pane(
    pane_id: &str,
    backends: &[(String, Vec<String>)],
) -> Option<BTreeSet<String>> {
    let pane_pid = pane_pid(pane_id)?;
    let process_tree = ProcessTree::snapshot()?;
    Some(matching_backends_for_process_names(
        process_tree.descendant_names(pane_pid),
        backends,
    ))
}

// A `check_known_app` helper used to run here. It compared
// `#{pane_current_command}` against a list of expected app names and warned
// when there was no match, but its only caller passed an empty list, so the
// match could never succeed and the warning fired on every single injection —
// including for panes genuinely running the assistant. It was removed rather
// than repaired: `pane_current_command` reports the *foreground child*, so a
// busy agent running a tool command reads as `bash` and the signal is
// unreliable for exactly the sessions that matter most. `backends_in_pane`
// already walks the pane's process tree when a trustworthy answer is needed.

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
) -> anyhow::Result<InjectVerification> {
    let t0 = Instant::now();

    if vim_mode {
        if let Some(pattern) = tui_pattern {
            ensure_insert_mode(pane, pattern)?;
        }
    }
    let t2 = Instant::now();

    inject_text(pane, message, config)?;
    let t3 = Instant::now();

    // The paste commands succeeded, but that only proves tmux accepted them.
    // Verification is the only evidence that the target actually received the
    // text, so its result is returned to the caller instead of being dropped.
    thread::sleep(Duration::from_millis(VERIFY_DELAY_MS));
    let verification = verify_injected(pane, message);
    let t4 = Instant::now();

    tracing::info!(
        pane,
        msg_len = message.len(),
        vim_mode_ms = t2.duration_since(t0).as_millis() as u64,
        inject_ms = t3.duration_since(t2).as_millis() as u64,
        verify_ms = t4.duration_since(t3).as_millis() as u64,
        total_ms = t4.duration_since(t0).as_millis() as u64,
        "inject timing"
    );

    Ok(verification)
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

/// Result of checking whether injected text actually reached the pane.
///
/// `Unconfirmed` deliberately does not mean "failed": the capture window is a
/// lossy observation of a live TUI. It means the daemon has no evidence the
/// text arrived, which callers must report as ambiguous rather than as a
/// successful delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InjectVerification {
    /// The injected text was observed in the pane after the paste.
    Confirmed,
    /// The paste commands succeeded but the text was not observed.
    Unconfirmed(String),
}

/// Check whether the injected text is visible in the pane after the paste.
///
/// The needle is taken from the **tail** of the sanitized message: after
/// submission the most recently pasted text is the part still on screen, while
/// a long message legitimately scrolls its own beginning out of the capture
/// window. Both sides are normalized (whitespace and box-drawing glyphs
/// removed) so that TUI line wrapping and input-box borders inserted in the
/// middle of the pasted text do not, by themselves, cause a miss.
///
/// A miss is still not proof of loss — a TUI that elides or reflows the user's
/// message in a way this normalization does not cover would also miss. That
/// residual false-negative risk is why callers map `Unconfirmed` to
/// `DeliveryOutcome::Ambiguous` and never to `Rejected`.
fn verify_injected(pane: &str, message: &str) -> InjectVerification {
    let content = match capture_pane(pane) {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("could not capture pane {pane} to verify injection: {e}");
            // Logged quietly on purpose: the caller decides how loud a miss is,
            // because only it knows whether the recipient was mid-turn.
            tracing::debug!(pane, "inject verification: {reason}");
            return InjectVerification::Unconfirmed(reason);
        }
    };

    let sanitized = sanitize_injection_text(message);
    let needle = verification_needle(&sanitized);

    if normalize_for_verification(&content).contains(&normalize_for_verification(needle)) {
        return InjectVerification::Confirmed;
    }

    let reason = format!(
        "injected text was not observed in pane {pane} after paste \
         (checked the last {VERIFY_NEEDLE_LEN} characters of the message \
         against the last {} captured lines)",
        CAPTURE_SCROLL_LINES.trim_start_matches('-')
    );
    // Quiet by design. A miss right after the paste is expected while the
    // recipient is mid-turn, so escalating here would warn on nearly every
    // message to a busy session. The caller warns once it knows the recipient
    // was idle, and the deferred re-check warns when the text is still absent
    // after the recipient's turn ended.
    tracing::debug!(
        pane,
        msg_len = message.len(),
        "inject verification: {reason}"
    );
    InjectVerification::Unconfirmed(reason)
}

/// An injection whose first verification missed while the recipient was
/// mid-turn, held for re-verification once that turn ends.
///
/// This exists so that "the TUI had not redrawn yet" never becomes a silent
/// pass: every queued delivery is looked at again, and a still-absent message
/// is reported as a real loss.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeferredInjectVerification {
    /// Public session id of the recipient.
    pub session_id: String,
    /// Pane the text was pasted into.
    pub pane: String,
    /// The exact text that was injected; re-verified with the same needle.
    pub message: String,
    /// Message id reported to the sender, when the send carried one.
    pub msg_id: Option<u64>,
    /// Why the first verification was inconclusive.
    pub first_reason: String,
}

#[cfg(test)]
static TEST_DEFERRED_VERIFICATION: std::sync::Mutex<Option<InjectVerification>> =
    std::sync::Mutex::new(None);

/// Test hook: force the result of the next deferred re-verification.
///
/// Unit tests must not capture panes on the host tmux server, so the re-check's
/// tmux side is replaced while its decision and reporting logic stay live.
#[cfg(test)]
pub(crate) fn set_test_deferred_verification(verification: Option<InjectVerification>) {
    *TEST_DEFERRED_VERIFICATION
        .lock()
        .expect("test deferred verification lock") = verification;
}

/// Re-capture the recipient's pane and look for the injected text again.
fn verify_deferred_injection(pending: &DeferredInjectVerification) -> InjectVerification {
    #[cfg(test)]
    {
        let _ = pending;
        return TEST_DEFERRED_VERIFICATION
            .lock()
            .expect("test deferred verification lock")
            .clone()
            .unwrap_or(InjectVerification::Confirmed);
    }
    #[cfg(not(test))]
    {
        thread::sleep(Duration::from_millis(DEFERRED_VERIFY_DELAY_MS));
        verify_injected(&pending.pane, &pending.message)
    }
}

/// Decide what a deferred re-check result means.
///
/// Pure. `Some(reason)` is a genuine loss: the recipient has finished the turn
/// that could explain the first miss, and the text is still not in its pane.
pub(crate) fn deferred_delivery_loss(
    pending: &DeferredInjectVerification,
    verification: &InjectVerification,
) -> Option<String> {
    match verification {
        InjectVerification::Confirmed => None,
        InjectVerification::Unconfirmed(reason) => Some(format!(
            "message was still not observed in pane {} after session {} finished its turn; \
             treat it as lost, not queued (first check: {}; re-check: {reason})",
            pending.pane, pending.session_id, pending.first_reason
        )),
    }
}

/// Run a deferred re-check and report a still-missing message at warn.
///
/// Returns the loss reason when the message is gone, `None` when it landed.
/// Blocking: call from a blocking context.
pub(crate) fn report_deferred_injection(pending: &DeferredInjectVerification) -> Option<String> {
    let verification = verify_deferred_injection(pending);
    match deferred_delivery_loss(pending, &verification) {
        Some(reason) => {
            tracing::warn!(
                session = %pending.session_id,
                pane = %pending.pane,
                msg_id = ?pending.msg_id,
                "queued message never arrived: {reason}"
            );
            Some(reason)
        }
        None => {
            tracing::debug!(
                session = %pending.session_id,
                pane = %pending.pane,
                msg_id = ?pending.msg_id,
                "queued message confirmed in pane after the recipient's turn ended"
            );
            None
        }
    }
}

/// Strip characters a TUI may insert into the middle of pasted text.
///
/// Line wrapping adds newlines and padding; input boxes add vertical borders.
/// Removing whitespace and box-drawing glyphs from both the capture and the
/// needle keeps the comparison about the message's own characters.
fn normalize_for_verification(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '\u{2500}'..='\u{257f}'))
        .collect()
}

/// Take the last [`VERIFY_NEEDLE_LEN`] characters of `message`, on a char boundary.
fn verification_needle(message: &str) -> &str {
    if message.len() <= VERIFY_NEEDLE_LEN {
        return message;
    }

    let start = message
        .char_indices()
        .rev()
        .map(|(idx, _)| idx)
        .find(|idx| message.len() - idx >= VERIFY_NEEDLE_LEN)
        .unwrap_or(0);
    &message[start..]
}

fn next_inject_buffer_name(pane: &str) -> String {
    let seq = INJECT_BUFFER_SEQ.fetch_add(1, Ordering::Relaxed);
    let pane_id: String = pane.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!("ouija-inject-{pane_id}-{seq}")
}

fn paste_buffer_args<'a>(
    buffer_name: &'a str,
    pane: &'a str,
    use_bracketed_paste: bool,
) -> Vec<&'a str> {
    let mut args = vec!["paste-buffer"];
    if use_bracketed_paste {
        args.push("-p");
    }
    args.extend(["-b", buffer_name, "-t", pane]);
    args
}

/// Inject message text via `tmux paste-buffer` then submit with Enter.
///
/// When `config.use_inner_bracketed_paste` is true, `paste-buffer -p` wraps
/// the text in bracketed paste sequences (`ESC[200~...ESC[201~`) when the TUI
/// has requested bracketed-paste mode.
/// This is necessary for TUIs that intercept individual keystrokes from
/// `send-keys -l`, silently swallowing them. Delegating the framing to tmux
/// keeps the control bytes out of the paste buffer, where tmux 3.7 would
/// otherwise sanitize them into visible `^[[200~` and `^[[201~` text.
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
    let paste_content = sanitize_injection_text(message);

    let buffer_name = next_inject_buffer_name(pane);

    // Load into a named tmux paste buffer via stdin. The unnamed buffer is
    // global to the tmux server, so concurrent injections into different panes
    // must never share it.
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-b", &buffer_name, "-"])
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

    // Let tmux add real bracketed-paste controls outside the sanitized buffer.
    let status = Command::new("tmux")
        .args(paste_buffer_args(
            &buffer_name,
            pane,
            config.use_inner_bracketed_paste,
        ))
        .status()
        .context("failed to run tmux paste-buffer")?;

    let delete_status = Command::new("tmux")
        .args(["delete-buffer", "-b", &buffer_name])
        .status()
        .context("failed to run tmux delete-buffer")?;

    if !delete_status.success() {
        tracing::warn!(buffer = %buffer_name, "tmux delete-buffer failed after injection");
    }

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
    pub(crate) owned_assistant_process: Option<OwnedAssistantProcessEvidence>,
    pub result_tx: tokio::sync::oneshot::Sender<anyhow::Result<InjectVerification>>,
}

#[derive(Clone, Debug)]
pub(crate) struct OwnedAssistantProcessEvidence {
    pub owner: crate::daemon_protocol::ResourceOwner,
    pub process_names: Vec<String>,
}

fn validate_owned_assistant_process(
    pane: &str,
    evidence: &OwnedAssistantProcessEvidence,
) -> anyhow::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    let observed = inspect_pane_owner(pane)?
        .ok_or_else(|| anyhow::anyhow!("scheduled pane has no physical owner"))?;
    if !physical_owner_matches(&observed, &evidence.owner) {
        anyhow::bail!("scheduled pane physical owner changed");
    }
    let names = evidence
        .process_names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    if !pane_alive(pane, &names) {
        anyhow::bail!("scheduled pane is no longer running its assistant process");
    }
    Ok(())
}

/// Background worker that drains the FIFO queue for a single pane.
///
/// Messages are processed in order. On failure, retries with exponential
/// backoff before reporting the error back to the caller.
pub async fn pane_inject_loop(mut rx: tokio::sync::mpsc::UnboundedReceiver<InjectRequest>) {
    while let Some(req) = rx.recv().await {
        let mut attempts = 0u32;
        let result = loop {
            if let Some(evidence) = req.owned_assistant_process.as_ref()
                && let Err(error) = validate_owned_assistant_process(&req.pane, evidence)
            {
                break Err(error);
            }
            let pane = req.pane.clone();
            let message = req.message.clone();
            let vim_mode = req.vim_mode;
            let config = crate::backend::InjectConfig {
                paste_settle_ms: req.inject_config.paste_settle_ms,
                use_inner_bracketed_paste: req.inject_config.use_inner_bracketed_paste,
                startup_inject_delay_secs: req.inject_config.startup_inject_delay_secs,
            };
            let tui_pattern = req.tui_pattern.clone();
            // `inject` shells out to tmux via std::process::Command, which has
            // no timeout of its own. A tmux server that stops answering would
            // otherwise park this task forever, and every caller awaiting
            // `result_rx` with it. Bounding the wait converts an indefinite
            // stall into a retryable error. The blocking thread may outlive the
            // timeout — that is deliberate: leaking one pool thread is strictly
            // better than stalling delivery for every session.
            let attempt = tokio::time::timeout(
                INJECT_ATTEMPT_TIMEOUT,
                tokio::task::spawn_blocking(move || {
                    inject(&pane, &message, vim_mode, &config, tui_pattern.as_deref())
                }),
            )
            .await
            .unwrap_or_else(|_| {
                Ok(Err(anyhow::anyhow!(
                    "inject timed out after {}s (tmux did not respond)",
                    INJECT_ATTEMPT_TIMEOUT.as_secs()
                )))
            });

            match attempt {
                // An unverified inject is reported, not retried: the paste
                // itself succeeded, so retrying risks delivering the message
                // twice. The ambiguity is surfaced to the caller instead.
                Ok(Ok(verification)) => break Ok(verification),
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

    let backend = state.backend_or_default(metadata.backend.as_deref()).await;

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
pub async fn locked_inject_owned(
    state: &crate::state::AppState,
    owner: &crate::daemon_protocol::ResourceOwner,
    pane: &str,
    message: &str,
    vim_mode: bool,
) -> anyhow::Result<()> {
    state
        .with_owned_pane_claim(owner, pane, || async {
            match session_delivery_plan(state, &owner.session_id, pane).await {
                SessionDeliveryPlan::Http(delivery) => state
                    .with_owned_backend_claim(owner, &delivery.backend_session_id, || async {
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
                            tracing::warn!(
                                session = %owner.session_id,
                                ?decision,
                                "owned http delivery failed"
                            );
                        }
                        Ok(())
                    })
                    .await
                    .unwrap_or_else(|| {
                        Err(anyhow::anyhow!(
                            "session '{}' incarnation {} no longer owns backend session '{}'",
                            owner.session_id,
                            owner.incarnation,
                            delivery.backend_session_id
                        ))
                    }),
                SessionDeliveryPlan::RawTmux {
                    inject_config,
                    tui_pattern,
                } => {
                    locked_inject_raw_tmux_with_config(
                        state,
                        pane,
                        message,
                        vim_mode,
                        inject_config,
                        tui_pattern,
                    )
                    .await
                }
                SessionDeliveryPlan::Unavailable(reason) => Err(anyhow::anyhow!(reason)),
            }
        })
        .await
        .unwrap_or_else(|| {
            Err(anyhow::anyhow!(
                "session '{}' incarnation {} no longer owns pane '{pane}'",
                owner.session_id,
                owner.incarnation
            ))
        })
}

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
                owned_assistant_process: None,
                result_tx,
            };
            state.enqueue_inject(req);
            // Best-effort caller: an unverified inject is logged by
            // `verify_injected` and does not change this path's semantics.
            return result_rx
                .await
                .map_err(|_| anyhow::anyhow!("inject queue closed"))?
                .map(|_| ());
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
    locked_inject_raw_tmux_verified(state, session_id, pane, message, vim_mode)
        .await
        .map(|_| ())
}

/// Like [`locked_inject_raw_tmux`], but reports whether the injected text was
/// actually observed in the pane. Callers that report delivery status to a
/// sender must use this and must not treat `Unconfirmed` as success.
pub(crate) async fn locked_inject_raw_tmux_verified(
    state: &crate::state::AppState,
    session_id: &str,
    pane: &str,
    message: &str,
    vim_mode: bool,
) -> anyhow::Result<InjectVerification> {
    if cfg!(test) {
        return Ok(InjectVerification::Confirmed);
    }

    let backend = state.backend_for_session(session_id).await;
    let config = backend.inject_config();
    let tui_pattern = backend.tui_ready_pattern().map(String::from);

    locked_inject_raw_tmux_with_config_and_evidence(
        state,
        pane,
        message,
        vim_mode,
        config,
        tui_pattern,
        None,
    )
    .await
}

pub async fn locked_inject_raw_tmux_with_config(
    state: &crate::state::AppState,
    pane: &str,
    message: &str,
    vim_mode: bool,
    inject_config: crate::backend::InjectConfig,
    tui_pattern: Option<String>,
) -> anyhow::Result<()> {
    locked_inject_raw_tmux_with_config_and_evidence(
        state,
        pane,
        message,
        vim_mode,
        inject_config,
        tui_pattern,
        None,
    )
    .await
    .map(|_| ())
}

pub(crate) async fn locked_inject_raw_tmux_with_config_and_evidence(
    state: &crate::state::AppState,
    pane: &str,
    message: &str,
    vim_mode: bool,
    inject_config: crate::backend::InjectConfig,
    tui_pattern: Option<String>,
    owned_assistant_process: Option<OwnedAssistantProcessEvidence>,
) -> anyhow::Result<InjectVerification> {
    if cfg!(test) {
        return Ok(InjectVerification::Confirmed);
    }

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let req = InjectRequest {
        pane: pane.to_string(),
        message: message.to_string(),
        vim_mode,
        inject_config,
        tui_pattern,
        owned_assistant_process,
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

/// Configure tmux options for panes/windows managed by ouija.
pub fn configure_managed_pane(pane_id: &str) {
    if cfg!(test) {
        return;
    }

    // Keep ouija's window name stable, but do not preserve dead panes.
    let _ = Command::new("tmux")
        .args([
            "set-window-option",
            "-t",
            pane_id,
            "automatic-rename",
            "off",
        ])
        .status();
    let _ = Command::new("tmux")
        .args(["set-window-option", "-t", pane_id, "remain-on-exit", "off"])
        .status();
}

/// Wrap a command pasted into an interactive shell so the shell exits after it.
pub fn close_shell_after(command: &str) -> String {
    format!("{command}; exit")
}

/// Resolve the shell tmux itself is configured to launch for a new pane.
///
/// `respawn-pane` without a shell-command reuses the pane's original command,
/// which may be a backend binary for externally registered panes. Callers
/// that need a fresh interactive command boundary must pass this explicitly.
pub fn default_shell() -> String {
    Command::new("tmux")
        .args(["show-options", "-gv", "default-shell"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let shell = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!shell.is_empty()).then_some(shell)
        })
        .or_else(|| {
            std::env::var("SHELL")
                .ok()
                .filter(|shell| !shell.is_empty())
        })
        .unwrap_or_else(|| "/bin/sh".into())
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
///   1. The `@ouija_session` tmux pane var is set only after registration
///      commits, and a newly allocated pane can take a short time to become
///      visible to the guarded marker writer.
///   2. Opencode bash subshells occasionally do not inherit `TMUX_PANE`.
///   3. Sessions launched outside tmux (future non-tmux backends) have no
///      pane var to read at all.
///
/// `OUIJA_SESSION_INCARNATION`, when supplied, identifies the exact lifecycle
/// owner of the managed launch. `OUIJA_SESSION_START_CREDENTIAL`, when supplied
/// for a managed TUI launch, authorizes only its first backend-identity binding.
/// `HISTFILE=/dev/null` and `fish_history=` suppress history writes so ouija
/// commands don't pollute the user's shell history.
pub fn pane_env_args(
    session_id: &str,
    session_start_credential: Option<&str>,
    session_incarnation: Option<crate::daemon_protocol::SessionIncarnation>,
) -> Vec<String> {
    let mut args = vec![
        "-e".into(),
        format!("OUIJA_SESSION_ID={session_id}"),
        "-e".into(),
        "HISTFILE=/dev/null".into(),
        "-e".into(),
        "fish_history=".into(),
    ];
    if let Some(credential) = session_start_credential {
        args.extend([
            "-e".into(),
            format!("OUIJA_SESSION_START_CREDENTIAL={credential}"),
        ]);
    }
    if let Some(incarnation) = session_incarnation {
        args.extend([
            "-e".into(),
            format!("OUIJA_SESSION_INCARNATION={incarnation}"),
        ]);
    }
    args
}

fn process_environment_value(environment: &[u8], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    environment.split(|byte| *byte == 0).find_map(|entry| {
        let entry = std::str::from_utf8(entry).ok()?;
        entry.strip_prefix(&prefix).map(String::from)
    })
}

fn tmux_pane_is_absent(stderr: &str) -> bool {
    stderr.contains("can't find pane")
        || stderr.contains("no server running")
        || stderr.contains("failed to connect to server")
        || stderr.contains("error connecting to ")
}

fn tmux_target_pane_is_absent(stderr: &str) -> bool {
    stderr.contains("can't find pane")
}

fn inspect_pane_format_with(
    pane: &str,
    format: &str,
    pane_is_absent: fn(&str) -> bool,
) -> anyhow::Result<Option<String>> {
    match std::process::Command::new("tmux")
        .args(["display-message", "-p", "-t", pane, format])
        .output()
    {
        Ok(output) if output.status.success() => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if pane_is_absent(&stderr) {
                Ok(None)
            } else {
                anyhow::bail!("tmux pane inspection failed for {pane}: {}", stderr.trim());
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn inspect_pane_format(pane: &str, format: &str) -> anyhow::Result<Option<String>> {
    match inspect_pane_format_with(pane, format, tmux_pane_is_absent) {
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        result => result,
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ManagedPaneInspection {
    Missing,
    Unmanaged,
    /// Complete launch owner exported by the pane's current process.
    ProcessOwner(crate::daemon_protocol::ResourceOwner),
    /// Complete owner recovered only from mutable tmux pane markers.
    MarkerOwner(crate::daemon_protocol::ResourceOwner),
}

impl ManagedPaneInspection {
    pub(crate) fn owner(&self) -> Option<&crate::daemon_protocol::ResourceOwner> {
        match self {
            Self::ProcessOwner(owner) | Self::MarkerOwner(owner) => Some(owner),
            Self::Missing | Self::Unmanaged => None,
        }
    }
}

pub(crate) fn pane_accepts_owner_marker(
    inspection: &ManagedPaneInspection,
    expected: &crate::daemon_protocol::ResourceOwner,
) -> bool {
    match inspection {
        ManagedPaneInspection::Unmanaged => true,
        ManagedPaneInspection::ProcessOwner(observed)
        | ManagedPaneInspection::MarkerOwner(observed) => {
            physical_owner_matches(observed, expected)
        }
        ManagedPaneInspection::Missing => false,
    }
}

/// Decide whether the daemon may write an exact current owner's pane markers.
///
/// Process environments and tmux markers can outlive an engine conversation.
/// Protocol state decides whether either observed owner still has authority.
pub(crate) fn pane_marker_write_is_authorized(
    inspection: &ManagedPaneInspection,
    expected: &crate::daemon_protocol::ResourceOwner,
    observed_owner_blocks_reassignment: bool,
) -> bool {
    match inspection {
        ManagedPaneInspection::Unmanaged => true,
        ManagedPaneInspection::ProcessOwner(observed) => {
            physical_owner_matches(observed, expected) || !observed_owner_blocks_reassignment
        }
        ManagedPaneInspection::MarkerOwner(observed) => {
            physical_owner_matches(observed, expected) || !observed_owner_blocks_reassignment
        }
        ManagedPaneInspection::Missing => false,
    }
}

fn parse_owner(
    session_id: &str,
    incarnation: &str,
    source: &str,
) -> anyhow::Result<Option<crate::daemon_protocol::ResourceOwner>> {
    if session_id.is_empty() || incarnation.is_empty() {
        return Ok(None);
    }
    let incarnation = incarnation.parse::<u64>().map_err(|error| {
        anyhow::anyhow!("invalid OUIJA_SESSION_INCARNATION from {source}: {incarnation:?}: {error}")
    })?;
    Ok(Some(crate::daemon_protocol::ResourceOwner {
        session_id: session_id.to_string(),
        incarnation: crate::daemon_protocol::SessionIncarnation(incarnation),
    }))
}

/// Inspect the exact lifecycle owner exported into a live managed pane.
///
/// `Ok(None)` means the pane is absent, unmanaged, or belongs to another
/// launch shape. Unexpected tmux inspection failures remain errors so startup
/// recovery can retain the lease and fail closed.
pub fn inspect_pane_owner(
    pane: &str,
) -> anyhow::Result<Option<crate::daemon_protocol::ResourceOwner>> {
    Ok(inspect_managed_pane(pane)?.owner().cloned())
}

/// Distinguish a missing pane from a live pane without managed identity.
pub(crate) fn inspect_managed_pane(pane: &str) -> anyhow::Result<ManagedPaneInspection> {
    inspect_managed_pane_with(pane, false)
}

/// Inspect a reclaim incumbent without treating daemon/socket failures as absence.
pub(crate) fn inspect_managed_pane_for_reclaim(
    pane: &str,
) -> anyhow::Result<ManagedPaneInspection> {
    inspect_managed_pane_with(pane, true)
}

fn inspect_managed_pane_with(
    pane: &str,
    strict_missing: bool,
) -> anyhow::Result<ManagedPaneInspection> {
    let inspect = |format| {
        if strict_missing {
            inspect_pane_format_with(pane, format, tmux_target_pane_is_absent)
        } else {
            inspect_pane_format(pane, format)
        }
    };
    let Some(pane_pid) = inspect("#{pane_pid}")? else {
        return Ok(ManagedPaneInspection::Missing);
    };
    if pane_pid.is_empty() {
        // Some tmux versions return success with an empty format expansion
        // when the target pane disappeared. A live pane always has a PID.
        return Ok(ManagedPaneInspection::Missing);
    }

    // Prefer the current process environment when it is available. A managed
    // respawn replaces the process before its pane markers are refreshed, so
    // the process carries the newer incarnation during that transition.
    #[cfg(target_os = "linux")]
    let environment = match std::fs::read(format!("/proc/{pane_pid}/environ")) {
        Ok(environment) => environment,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect environment for pane {pane} pid {pane_pid}")
            });
        }
    };
    #[cfg(not(target_os = "linux"))]
    let environment = {
        let output = std::process::Command::new("ps")
            .args(["eww", "-p", &pane_pid, "-o", "command="])
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "failed to inspect environment for pane {pane} pid {pane_pid}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        output
            .stdout
            .split(|byte| byte.is_ascii_whitespace())
            .flat_map(|entry| entry.iter().copied().chain(std::iter::once(0)))
            .collect::<Vec<_>>()
    };

    let process_session_id =
        process_environment_value(&environment, "OUIJA_SESSION_ID").unwrap_or_default();
    let process_incarnation =
        process_environment_value(&environment, "OUIJA_SESSION_INCARNATION").unwrap_or_default();
    if let Some(owner) = parse_owner(
        &process_session_id,
        &process_incarnation,
        &format!("pane {pane} process"),
    )? {
        return Ok(ManagedPaneInspection::ProcessOwner(owner));
    }

    // HTTP-backed sessions return to their persistent shell after the backend
    // attach exits. That shell may not expose the launch environment, while
    // these daemon-owned pane markers remain bound to the physical pane.
    let Some(marker_session_id) = inspect("#{@ouija_id}")? else {
        return Ok(ManagedPaneInspection::Missing);
    };
    let Some(marker_incarnation) = inspect("#{@ouija_incarnation}")? else {
        return Ok(ManagedPaneInspection::Missing);
    };
    if let Some(owner) = parse_owner(
        &marker_session_id,
        &marker_incarnation,
        &format!("pane {pane} markers"),
    )? {
        return Ok(ManagedPaneInspection::MarkerOwner(owner));
    }

    if inspect("#{pane_pid}")?.is_none_or(|pid| pid.is_empty()) {
        return Ok(ManagedPaneInspection::Missing);
    }

    Ok(ManagedPaneInspection::Unmanaged)
}

/// Process environments are immutable across a logical session rename. The
/// allocator-issued incarnation remains globally unique, so it is the durable
/// physical pane identity while protocol ownership still compares the full
/// `(session_id, incarnation)` pair.
pub(crate) fn physical_owner_matches(
    observed: &crate::daemon_protocol::ResourceOwner,
    expected: &crate::daemon_protocol::ResourceOwner,
) -> bool {
    observed.incarnation == expected.incarnation
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
    use std::collections::BTreeSet;

    fn deferred(pane: &str) -> DeferredInjectVerification {
        DeferredInjectVerification {
            session_id: "worker".into(),
            pane: pane.into(),
            message: "hello".into(),
            msg_id: Some(7),
            first_reason: "recipient was mid-turn".into(),
        }
    }

    #[test]
    fn deferred_recheck_that_still_misses_reports_a_loss() {
        let loss = deferred_delivery_loss(
            &deferred("%3"),
            &InjectVerification::Unconfirmed("not observed in pane %3".into()),
        )
        .expect("a still-absent message after the turn ended is a loss");

        assert!(loss.contains("pane %3"), "loss must name the pane: {loss}");
        assert!(
            loss.contains("session worker"),
            "loss must name the session: {loss}"
        );
    }

    #[test]
    fn deferred_recheck_that_finds_the_message_is_silent() {
        assert_eq!(
            deferred_delivery_loss(&deferred("%3"), &InjectVerification::Confirmed),
            None
        );
    }

    #[test]
    fn matching_backends_returns_all_matches() {
        let backends = vec![
            ("claude-code".to_string(), vec!["claude".to_string()]),
            ("opencode".to_string(), vec!["opencode".to_string()]),
        ];

        let matches =
            matching_backends_for_process_names(["bash", ".opencode", "claude"], &backends);

        assert_eq!(
            matches,
            BTreeSet::from(["claude-code".to_string(), "opencode".to_string()])
        );
    }

    #[test]
    fn matching_backends_returns_empty_for_unknown_processes() {
        let backends = vec![
            ("claude-code".to_string(), vec!["claude".to_string()]),
            ("opencode".to_string(), vec!["opencode".to_string()]),
        ];

        let matches = matching_backends_for_process_names(["bash", "vim"], &backends);

        assert!(matches.is_empty());
    }

    #[test]
    fn renamed_owner_matches_immutable_pane_incarnation() {
        let observed = crate::daemon_protocol::ResourceOwner {
            session_id: "before".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(42),
        };
        let renamed = crate::daemon_protocol::ResourceOwner {
            session_id: "after".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(42),
        };
        let replacement = crate::daemon_protocol::ResourceOwner {
            session_id: "after".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(43),
        };

        assert!(physical_owner_matches(&observed, &renamed));
        assert!(!physical_owner_matches(&observed, &replacement));
    }

    #[test]
    fn pane_env_args_includes_ouija_session_id() {
        let args = pane_env_args("feat/442-chunk-4", None, None);
        // Flat -e KEY=VALUE pairs, in order, suitable for splatting into tmux argv
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "OUIJA_SESSION_ID=feat/442-chunk-4"),
            "expected OUIJA_SESSION_ID=<id> in args, got {args:?}"
        );
    }

    #[test]
    fn pane_env_args_includes_managed_launch_credential_when_supplied() {
        let args = pane_env_args("feat/442", Some("credential"), None);
        assert!(
            args.windows(2)
                .any(|w| { w[0] == "-e" && w[1] == "OUIJA_SESSION_START_CREDENTIAL=credential" }),
            "expected launch credential in pane env, got {args:?}"
        );
    }

    #[test]
    fn pane_env_args_includes_session_incarnation() {
        let args = pane_env_args(
            "feat/1952",
            None,
            Some(crate::daemon_protocol::SessionIncarnation(42)),
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "OUIJA_SESSION_INCARNATION=42"),
            "expected session incarnation in pane env, got {args:?}"
        );
    }

    #[test]
    fn pane_env_args_preserves_history_suppression() {
        let args = pane_env_args("x", None, None);
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
        let args = pane_env_args("abc", None, None);
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
    fn process_environment_value_extracts_nul_delimited_variable() {
        assert_eq!(
            process_environment_value(
                b"OTHER=x\0OUIJA_SESSION_INCARNATION=42\0",
                "OUIJA_SESSION_INCARNATION"
            )
            .as_deref(),
            Some("42")
        );
    }

    #[test]
    fn missing_tmux_socket_reports_pane_as_absent() {
        assert!(tmux_pane_is_absent(
            "error connecting to /tmp/tmux-1001/default (No such file or directory)"
        ));
        assert!(!tmux_pane_is_absent("permission denied"));
    }

    #[test]
    fn strict_reclaim_inspection_only_accepts_an_explicitly_missing_target_pane() {
        assert!(tmux_target_pane_is_absent("can't find pane: %439"));
        assert!(!tmux_target_pane_is_absent(
            "no server running on /tmp/tmux"
        ));
        assert!(!tmux_target_pane_is_absent(
            "failed to connect to server: permission denied"
        ));
        assert!(!tmux_target_pane_is_absent(
            "error connecting to /tmp/tmux/default"
        ));
        assert!(!tmux_target_pane_is_absent("permission denied"));
    }

    #[test]
    fn pane_marker_owner_survives_missing_process_identity() {
        let owner = parse_owner("oc-e2e", "42", "test marker").unwrap().unwrap();
        assert_eq!(owner.session_id, "oc-e2e");
        assert_eq!(
            owner.incarnation,
            crate::daemon_protocol::SessionIncarnation(42)
        );
        assert!(parse_owner("", "42", "partial marker").unwrap().is_none());
        assert!(
            parse_owner("oc-e2e", "", "partial marker")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unmanaged_local_pane_accepts_first_owner_marker_but_replacement_does_not() {
        let expected = crate::daemon_protocol::ResourceOwner {
            session_id: "oc-e2e".to_string(),
            incarnation: crate::daemon_protocol::SessionIncarnation(42),
        };
        let replacement = crate::daemon_protocol::ResourceOwner {
            session_id: "oc-e2e".to_string(),
            incarnation: crate::daemon_protocol::SessionIncarnation(43),
        };

        assert!(pane_accepts_owner_marker(
            &ManagedPaneInspection::Unmanaged,
            &expected
        ));
        assert!(pane_accepts_owner_marker(
            &ManagedPaneInspection::ProcessOwner(expected.clone()),
            &expected
        ));
        assert!(!pane_accepts_owner_marker(
            &ManagedPaneInspection::MarkerOwner(replacement),
            &expected
        ));
        assert!(!pane_accepts_owner_marker(
            &ManagedPaneInspection::Missing,
            &expected
        ));
    }

    #[test]
    fn stale_pane_owners_can_be_reclaimed_only_without_live_authority() {
        let expected = crate::daemon_protocol::ResourceOwner {
            session_id: "hub".to_string(),
            incarnation: crate::daemon_protocol::SessionIncarnation(43),
        };
        let stale = crate::daemon_protocol::ResourceOwner {
            session_id: "hub".to_string(),
            incarnation: crate::daemon_protocol::SessionIncarnation(42),
        };

        assert!(pane_marker_write_is_authorized(
            &ManagedPaneInspection::MarkerOwner(stale.clone()),
            &expected,
            false,
        ));
        assert!(!pane_marker_write_is_authorized(
            &ManagedPaneInspection::MarkerOwner(stale.clone()),
            &expected,
            true,
        ));
        assert!(pane_marker_write_is_authorized(
            &ManagedPaneInspection::ProcessOwner(stale.clone()),
            &expected,
            false,
        ));
        assert!(!pane_marker_write_is_authorized(
            &ManagedPaneInspection::ProcessOwner(stale),
            &expected,
            true,
        ));
    }

    #[test]
    fn close_shell_after_appends_exit() {
        assert_eq!(
            close_shell_after("claude --danger"),
            "claude --danger; exit"
        );
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

    #[test]
    fn bracketed_paste_delegates_control_framing_to_tmux() {
        assert_eq!(
            paste_buffer_args("ouija-buffer", "%12", true),
            ["paste-buffer", "-p", "-b", "ouija-buffer", "-t", "%12"]
        );
        assert_eq!(
            paste_buffer_args("ouija-buffer", "%12", false),
            ["paste-buffer", "-b", "ouija-buffer", "-t", "%12"]
        );
    }

    #[test]
    fn verification_needle_stops_on_utf8_character_boundary() {
        let message = format!("prefix 🙂{}", "a".repeat(VERIFY_NEEDLE_LEN - 1));

        let needle = verification_needle(&message);

        // A 60-byte tail would split the emoji, so the needle grows to the
        // next character boundary instead.
        assert_eq!(needle, format!("🙂{}", "a".repeat(VERIFY_NEEDLE_LEN - 1)));
        assert!(message.is_char_boundary(message.len() - needle.len()));
        assert!(needle.len() >= VERIFY_NEEDLE_LEN);
    }

    #[test]
    fn verification_needle_uses_message_tail_not_head() {
        let message = format!("{}TAIL-MARKER", "h".repeat(VERIFY_NEEDLE_LEN * 2));

        let needle = verification_needle(&message);

        assert!(
            needle.ends_with("TAIL-MARKER"),
            "needle should come from the end of the message: {needle:?}"
        );
        assert!(message.ends_with(needle));
    }

    #[test]
    fn verification_needle_returns_whole_short_message() {
        assert_eq!(verification_needle("short message"), "short message");
    }

    #[test]
    fn verification_normalization_survives_tui_wrapping_and_borders() {
        let needle = "the quick brown fox jumps over the lazy dog";
        let wrapped = "\u{2502} the quick brown fox jumps \u{2502}\n\u{2502} over the lazy dog       \u{2502}";

        assert!(
            normalize_for_verification(wrapped).contains(&normalize_for_verification(needle)),
            "wrapped pane content should still match the needle"
        );
    }

    #[test]
    fn verification_normalization_still_rejects_absent_text() {
        let needle = "the quick brown fox";
        let pane = "\u{2502} an entirely different line \u{2502}";

        assert!(!normalize_for_verification(pane).contains(&normalize_for_verification(needle)));
    }

    #[test]
    fn tmux_injection_buffer_names_are_unique_and_scoped() {
        let first = next_inject_buffer_name("%12");
        let second = next_inject_buffer_name("%12");

        assert_ne!(first, second);
        assert!(first.starts_with("ouija-inject-12-"));
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
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
    fn has_descendant_named_codex_under_node_wrapper() {
        // The `codex` launcher is an npx/node wrapper: the pane's foreground
        // process is `node`, and the real agent is a descendant `codex` vendor
        // binary. Detection must walk to the descendant (#1442).
        let tree = ProcessTree {
            children: [(1, vec![2]), (2, vec![3])].into_iter().collect(),
            names: [(1, "bash".into()), (2, "node".into()), (3, "codex".into())]
                .into_iter()
                .collect(),
        };
        assert!(tree.has_descendant_named(1, &["codex"]));
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
