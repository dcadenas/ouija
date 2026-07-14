use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{CodingAssistant, DeliveryMode, InjectConfig, ResumeOpts, StartOpts};

// --- Embedded Codex hook scripts ---
// Compiled into the binary so `ouija start-server` can bootstrap Codex hook
// integration without needing the source repo on disk. Codex-specific: they
// emit `hookSpecificOutput.additionalContext` / `{"continue":true}` per the
// Codex hook output contract, and Stop is turn-scoped (never unregisters).
mod embedded {
    pub const SCRIPT_REGISTER: &str = include_str!("../../scripts/codex/codex-register.sh");
    pub const SCRIPT_PROMPT_SUBMIT: &str =
        include_str!("../../scripts/codex/codex-prompt-submit.sh");
    pub const SCRIPT_STOP: &str = include_str!("../../scripts/codex/codex-stop.sh");
    /// The shared ouija skill, installed into Codex's skill-discovery path so a
    /// Codex session can activate it on incoming `<msg>` tags, exactly as Claude
    /// Code and OpenCode do. Codex tool shells may lose Ouija's tmux environment;
    /// sender validation therefore uses the generic backend-identity contract.
    pub const SKILL_MD: &str = include_str!("../../skills/ouija/SKILL.md");
}

/// Codex hook events wired by Ouija, paired with the script that handles each.
/// SessionStart registers the pane; UserPromptSubmit signals activity; Stop is
/// turn-scoped bookkeeping. There is deliberately no SessionEnd/unregister hook —
/// Codex has no such event and cleanup relies on pane/process liveness (#1442).
const HOOKS: &[(&str, &str, &str)] = &[
    (
        "SessionStart",
        "codex-register.sh",
        embedded::SCRIPT_REGISTER,
    ),
    (
        "UserPromptSubmit",
        "codex-prompt-submit.sh",
        embedded::SCRIPT_PROMPT_SUBMIT,
    ),
    ("Stop", "codex-stop.sh", embedded::SCRIPT_STOP),
];

/// Directory under CODEX_HOME where Ouija writes its hook scripts.
fn hooks_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("ouija-hooks")
}

/// Private, per-user storage for credentials which must cross Codex's shared
/// app-server boundary without becoming part of its command line or TOML.
fn launch_credential_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("ouija-launch-credentials")
}

/// Stage a one-time launch credential in a private file and return only its
/// path. The hook atomically claims and deletes this file before it submits the
/// credential to the daemon.
fn stage_launch_credential(codex_home: &Path, credential: &str) -> anyhow::Result<PathBuf> {
    let dir = launch_credential_dir(codex_home);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }

    for _ in 0..8 {
        let bytes: [u8; 16] = ::rand::random();
        let nonce = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = dir.join(format!("launch-{nonce}"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(credential.as_bytes()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(error.into());
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("could not allocate a unique Codex launch credential file")
}

/// Build the merged `hooks.json` content for Codex, layering Ouija's hook
/// entries onto any existing config without clobbering the user's own hooks.
///
/// Returns `None` when `existing` is present but does not parse as JSON — the
/// caller must then leave the file untouched rather than overwrite it, so a
/// routine `start-server` never silently discards user hook config (finding f3).
/// This is conservative even if Codex accepts a JSON superset: a serde parse
/// failure would not prove the user's file is actually invalid, so overwriting
/// it would be wrong. `None`/absent existing installs fresh onto `{}`.
///
/// Idempotent: an Ouija-owned group is identified by a `command` that lives under
/// `hooks_dir`. For each managed event we drop any prior Ouija group and append a
/// fresh one, so re-installs neither duplicate nor accumulate stale script paths.
/// Non-Ouija hooks (other events, and the user's own groups within a managed
/// event) are preserved verbatim.
fn merge_hooks_json(existing: Option<&str>, hooks_dir: &Path) -> Option<serde_json::Value> {
    let dir_prefix = format!("{}/", hooks_dir.display());
    let is_ouija_group = |group: &serde_json::Value| -> bool {
        group["hooks"].as_array().is_some_and(|inner| {
            inner.iter().any(|h| {
                h["command"]
                    .as_str()
                    .is_some_and(|c| c.starts_with(&dir_prefix))
            })
        })
    };

    let mut root: serde_json::Value = match existing {
        // Present but unparseable → refuse to touch it (return None).
        Some(s) => serde_json::from_str(s).ok()?,
        None => serde_json::json!({}),
    };
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let hooks = root
        .as_object_mut()
        .expect("root normalized to object")
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        *hooks = serde_json::json!({});
    }
    let hooks = hooks.as_object_mut().expect("hooks normalized to object");

    for (event, script, _) in HOOKS {
        let command = hooks_dir.join(script).display().to_string();
        let entry = hooks
            .entry((*event).to_string())
            .or_insert_with(|| serde_json::json!([]));
        let arr = match entry.as_array_mut() {
            Some(a) => a,
            None => {
                *entry = serde_json::json!([]);
                entry.as_array_mut().expect("just set to array")
            }
        };
        arr.retain(|g| !is_ouija_group(g));
        arr.push(serde_json::json!({
            "hooks": [ { "type": "command", "command": command } ]
        }));
    }
    Some(root)
}

/// Write the shared ouija skill into Codex's skill-discovery path
/// (`$CODEX_HOME/skills/ouija/SKILL.md`). Idempotent: only the `ouija` subdir is
/// created/overwritten, so unrelated user skills under `skills/` are untouched.
fn install_skill_to(codex_home: &Path) -> anyhow::Result<()> {
    let skill_dir = codex_home.join("skills/ouija");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(skill_dir.join("SKILL.md"), embedded::SKILL_MD)?;
    Ok(())
}

/// Write Ouija's Codex hook scripts and merged `hooks.json` under `codex_home`,
/// plus the shared ouija skill under `skills/ouija/`. Idempotent (see
/// [`merge_hooks_json`] and [`install_skill_to`]); scripts are made executable on
/// unix.
fn install_to(codex_home: &Path) -> anyhow::Result<()> {
    let dir = hooks_dir(codex_home);
    std::fs::create_dir_all(&dir)?;

    for (_, script, content) in HOOKS {
        let dest = dir.join(script);
        std::fs::write(&dest, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
        }
    }

    let hooks_path = codex_home.join("hooks.json");
    let existing = std::fs::read_to_string(&hooks_path).ok();
    match merge_hooks_json(existing.as_deref(), &dir) {
        Some(merged) => std::fs::write(&hooks_path, serde_json::to_string_pretty(&merged)?)?,
        // Existing hooks.json is present but does not parse. Leave it untouched
        // rather than silently discard the user's config; warn so it's visible.
        None => eprintln!(
            "warning: {} is not valid JSON — leaving it untouched. Ouija Codex hooks were not \
             merged; fix or remove the file and re-run to enable them.",
            hooks_path.display()
        ),
    }

    // Install the ouija skill so Codex can activate it on incoming `<msg>` tags,
    // matching Claude/OpenCode. Independent of the hooks.json merge above.
    install_skill_to(codex_home)?;
    Ok(())
}

/// Install Ouija's Codex hooks and skill into an explicit Codex home.
///
/// This is used when the daemon is configured to launch Codex sessions with an
/// isolated `CODEX_HOME` for custom model providers.
pub(crate) fn install_to_home(codex_home: &Path) -> anyhow::Result<()> {
    install_to(codex_home)
}

/// Best-effort install of Ouija's Codex hooks/skill for a configured home.
///
/// Session-specific Codex homes are useful for provider configs (for example a
/// Gemini sidecar). They also need the same hooks and skill as the default home
/// so SessionStart can register the pane on the mesh.
pub(crate) fn install_configured_home(codex_home: Option<&str>) {
    let Some(home) = codex_home.map(str::trim).filter(|h| !h.is_empty()) else {
        return;
    };
    let expanded = crate::state::expand_tilde(home);
    if let Err(e) = install_to_home(Path::new(&expanded)) {
        tracing::warn!("failed to install Codex hooks into configured codex_home: {e}");
    }
}

/// Recursively collect `*.jsonl` rollout files under `dir` (Codex nests session
/// logs as `sessions/YYYY/MM/DD/rollout-*.jsonl`). Missing/unreadable dirs are
/// skipped silently.
fn collect_rollout_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// Read only the first line of a rollout file (the `session_meta` record).
fn first_line(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    (!line.trim().is_empty()).then_some(line)
}

/// Find the most recent Codex session whose recorded cwd matches `project_dir`,
/// returning its session id, or `None` if none match.
///
/// Each rollout file's first line is a `session_meta` record carrying
/// `payload.session_id`, `payload.cwd`, and `payload.timestamp`; only that line
/// is read. Recency is by `payload.timestamp` (ISO-8601, lexically sortable).
/// `None` (absent dir or no cwd match) lets the caller fall back to
/// `codex resume --last`. The cwd match is exact: Ouija launches Codex with
/// `--cd <project_dir>`, so the recorded cwd is that same string.
fn latest_session_id_for_cwd(sessions_root: &Path, project_dir: &str) -> Option<String> {
    let mut files = Vec::new();
    collect_rollout_files(sessions_root, &mut files);

    let mut best: Option<(String, String)> = None; // (timestamp, session_id)
    for file in files {
        let Some(line) = first_line(&file) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            continue;
        }
        let payload = &v["payload"];
        if payload.get("cwd").and_then(|c| c.as_str()) != Some(project_dir) {
            continue;
        }
        let (Some(ts), Some(sid)) = (
            payload.get("timestamp").and_then(|t| t.as_str()),
            payload.get("session_id").and_then(|s| s.as_str()),
        ) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(best_ts, _)| ts > best_ts.as_str())
        {
            best = Some((ts.to_string(), sid.to_string()));
        }
    }
    best.map(|(_, sid)| sid)
}

/// Resolve CODEX_HOME, honoring the `CODEX_HOME` override before `~/.codex`,
/// matching the Codex CLI's own resolution order.
fn resolve_codex_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CODEX_HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".codex"))
}

/// Resolve the Codex project trust root for a launch cwd.
///
/// Codex's trust gate keys linked worktrees to the common repository root, not
/// the linked worktree path. For normal and linked worktrees, `git rev-parse
/// --git-common-dir` resolves to `<repo>/.git`; its parent is the root Codex
/// asks the user to trust. Non-git directories fall back to the launch cwd.
fn codex_trust_root(project_dir: &str) -> PathBuf {
    let project = Path::new(project_dir);
    git_common_dir(project)
        .map(|common_dir| trust_root_from_common_dir(&common_dir, project))
        .unwrap_or_else(|| project.to_path_buf())
}

fn git_common_dir(project_dir: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(project_dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let path = stdout.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn trust_root_from_common_dir(common_dir: &Path, fallback: &Path) -> PathBuf {
    if common_dir.file_name().and_then(|s| s.to_str()) == Some(".git") {
        if let Some(parent) = common_dir.parent() {
            return parent.to_path_buf();
        }
    }
    fallback.to_path_buf()
}

fn toml_basic_string_escape(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{:04X}", c as u32);
            }
            c => escaped.push(c),
        }
    }
    escaped
}

fn format_trust_config_flag(project_dir: &str) -> String {
    let trust_root = codex_trust_root(project_dir);
    let root = toml_basic_string_escape(&trust_root.to_string_lossy());
    let config = format!("projects={{\"{root}\"={{trust_level=\"trusted\"}}}}");
    format!(" -c {}", crate::scheduler::shell_escape(&config))
}

/// Autonomy flags for Ouija-launched Codex sessions.
///
/// `--dangerously-bypass-approvals-and-sandbox` gives Ouija-managed Codex
/// sessions the same full-power local-worker posture as Claude Code's
/// `bypassPermissions`: no per-command approval prompts and no Codex sandbox
/// boundary. Ouija still launches inside the selected Ouija/Hub worktree, but
/// that worktree is cwd/scoping, not isolation. This is intended for trusted
/// local automation now and for an external sandbox boundary (for example
/// Docker) later. `--no-alt-screen` keeps terminal scrollback so pane capture
/// and debugging work. Both flags are verified present on `codex` and
/// `codex resume` (#1442, #1445).
const AUTONOMY_FLAGS: &str = "--dangerously-bypass-approvals-and-sandbox --no-alt-screen";

/// Render a ` --model <X>` fragment for the codex CLI, or an empty string.
///
/// The value is shell-escaped so it embeds safely in the surrounding
/// `format!`-built command. Empty / whitespace-only values are treated as
/// absent — emitting `codex --model ''` would just fail at the CLI, so omitting
/// the flag is the safer default.
fn format_model_flag(model: Option<&str>) -> String {
    match model.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => format!(" --model {}", crate::scheduler::shell_escape(m)),
        None => String::new(),
    }
}

/// Render a Codex config override for reasoning effort.
///
/// Codex CLI has no `--effort` flag. Reasoning effort is a config key, so Ouija
/// maps its existing `effort` field to `-c model_reasoning_effort="<value>"`.
/// Values are left as a normalized passthrough because Codex documents some
/// levels as model-dependent; Codex owns final validation for the selected model.
fn format_reasoning_effort_config_flag(effort: Option<&str>) -> String {
    match effort.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => {
            let config = format!("model_reasoning_effort=\"{}\"", toml_basic_string_escape(e));
            format!(" -c {}", crate::scheduler::shell_escape(&config))
        }
        None => String::new(),
    }
}

fn format_codex_home_prefix(codex_home: Option<&str>) -> String {
    match codex_home.map(str::trim).filter(|h| !h.is_empty()) {
        Some(home) => {
            let expanded = crate::state::expand_tilde(home);
            format!("CODEX_HOME={} ", crate::scheduler::shell_escape(&expanded))
        }
        None => String::new(),
    }
}

const SESSION_FLAGS_HOOK_KEY: &str = "<session-flags>/config.toml:session_start:0:0";

/// Build the two session-flags overrides for a fresh managed Codex launch.
///
/// The first declares one extra SessionStart command which presents Ouija's
/// public launch ID and a private credential-file path. The second pre-trusts
/// the exact normalized handler identity required by Codex; the static user
/// hook remains unmodified. This lets a shared Codex app-server prove ownership
/// without inheriting pane-local environment variables or logging the proof.
pub(crate) fn format_session_start_hook_flags(
    codex_home: &str,
    launch_session_id: &str,
    launch_credential_file: &Path,
) -> String {
    let script = hooks_dir(Path::new(codex_home)).join("codex-register.sh");
    let command = format!(
        "{} --launch-session-id {} --launch-credential-file {}",
        crate::scheduler::shell_escape(&script.to_string_lossy()),
        crate::scheduler::shell_escape(launch_session_id),
        crate::scheduler::shell_escape(&launch_credential_file.to_string_lossy()),
    );
    // Codex hashes the TOML-normalized command handler. Optional TOML fields
    // are absent, while the hook engine supplies timeout=600 and async=false.
    let normalized = serde_json::json!({
        "event_name": "session_start",
        "hooks": [{
            "async": false,
            "command": command,
            "timeout": 600,
            "type": "command",
        }],
    });
    let canonical = canonical_json(&normalized);
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(&canonical).expect("JSON value serializes"));
    let trusted_hash = format!("sha256:{:x}", hasher.finalize());

    let handler = toml_basic_string_escape(&command);
    let hook_config =
        format!("hooks.SessionStart=[{{hooks=[{{type=\"command\",command=\"{handler}\"}}]}}]");
    let state_config =
        format!("hooks.state.\"{SESSION_FLAGS_HOOK_KEY}\".trusted_hash=\"{trusted_hash}\"");
    format!(
        " -c {} -c {}",
        crate::scheduler::shell_escape(&hook_config),
        crate::scheduler::shell_escape(&state_config),
    )
}

/// Append a proven paneless SessionStart hook to a fresh Codex command.
///
/// Callers must use this only for a freshly credentialed launch. A resume has
/// an immutable native binding and must not mint or present a new proof.
pub(crate) fn with_session_start_hook(
    command: String,
    codex_home: Option<&str>,
    launch_session_id: &str,
    launch_credential: &str,
) -> anyhow::Result<String> {
    let home = codex_home
        .map(crate::state::expand_tilde)
        .map(PathBuf::from)
        .or_else(resolve_codex_home)
        .unwrap_or_else(|| PathBuf::from(".codex"));
    let credential_file = stage_launch_credential(&home, launch_credential)?;
    Ok(format!(
        "{command}{}",
        format_session_start_hook_flags(
            &home.to_string_lossy(),
            launch_session_id,
            &credential_file
        )
    ))
}

fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                sorted.insert(key.clone(), canonical_json(&map[key]));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonical_json).collect())
        }
        value => value.clone(),
    }
}

#[derive(Debug)]
pub struct Codex;

impl CodingAssistant for Codex {
    fn name(&self) -> &str {
        "codex-cli"
    }

    fn cli_name(&self) -> &str {
        "codex"
    }

    fn process_names(&self) -> &[&str] {
        // The `codex` launcher is often an npx/node wrapper whose foreground
        // `pane_current_command` reads as `node`; the long-running agent is a
        // descendant `codex` vendor binary. Ouija's process-tree walks
        // (`find_assistant_panes`, `pane_alive`, `detect_backend_in_pane`)
        // match this descendant, so listing `codex` here is sufficient.
        &["codex"]
    }

    fn delivery_mode(&self) -> DeliveryMode {
        DeliveryMode::TuiInjection
    }

    fn build_start_command(&self, opts: &StartOpts) -> String {
        let escaped_dir = crate::scheduler::shell_escape(&opts.project_dir);
        let model = format_model_flag(opts.model.as_deref());
        let effort = format_reasoning_effort_config_flag(opts.effort.as_deref());
        let codex_home = format_codex_home_prefix(opts.codex_home.as_deref());
        let trust = format_trust_config_flag(&opts.project_dir);
        // WorktreeMode is intentionally ignored: Codex CLI has no verified
        // `--worktree` flag. Ouija sets up the worktree/cwd before launch and
        // Codex is started inside it.
        format!("cd {escaped_dir} && {codex_home}codex {AUTONOMY_FLAGS}{trust}{effort}{model}")
    }

    fn build_resume_command(&self, opts: &ResumeOpts) -> Option<String> {
        let escaped_dir = crate::scheduler::shell_escape(&opts.project_dir);
        let model = format_model_flag(opts.model.as_deref());
        let effort = format_reasoning_effort_config_flag(opts.effort.as_deref());
        let codex_home = format_codex_home_prefix(opts.codex_home.as_deref());
        let trust = format_trust_config_flag(&opts.project_dir);
        // `codex resume --last` is the documented non-picker path for
        // continuing the most recent session in this cwd; an explicit
        // SESSION_ID targets a specific thread. WorktreeMode is ignored as in
        // `build_start_command`.
        let target = match &opts.session_id {
            Some(sid) => crate::scheduler::shell_escape(sid),
            None => "--last".to_string(),
        };
        Some(format!(
            "cd {escaped_dir} && {codex_home}codex resume {target} {AUTONOMY_FLAGS}{trust}{effort}{model}"
        ))
    }

    fn detect_session_id(&self, project_dir: &str) -> Option<String> {
        // Codex records sessions globally under `$CODEX_HOME/sessions/YYYY/MM/DD`.
        // Resolve the most recent session whose recorded cwd matches this project
        // so resume can target it explicitly (`codex resume <id>`); `None` falls
        // back to `codex resume --last`.
        let sessions_root = resolve_codex_home()?.join("sessions");
        latest_session_id_for_cwd(&sessions_root, project_dir)
    }

    fn caller_session_id(&self) -> Option<String> {
        std::env::var("CODEX_THREAD_ID")
            .ok()
            .filter(|session_id| !session_id.is_empty())
    }

    fn tui_ready_pattern(&self) -> Option<&str> {
        // Codex's interactive prompt glyph is U+203A (SINGLE RIGHT-POINTING
        // ANGLE QUOTATION MARK), observed as the visible `›` prompt.
        Some("\u{203A}")
    }

    fn inject_config(&self) -> InjectConfig {
        // Verified against a live Codex 0.142.5 TUI pane (2026-07-05): inner
        // bracketed paste + a 300ms settle before Enter delivers text intact
        // (special chars/quotes preserved, no dropped keystrokes), multi-line
        // input sanitizes to spaces without premature submit, and Enter reliably
        // submits the composed prompt. The pane was interactive within ~2s, so
        // the 5s startup delay is a safe conservative margin. See followup #692.
        InjectConfig {
            paste_settle_ms: 300,
            use_inner_bracketed_paste: true,
            startup_inject_delay_secs: 5,
        }
    }

    fn config_dir_name(&self) -> &str {
        ".codex"
    }

    fn has_project_history(&self, _dir: &Path) -> bool {
        // Codex keeps session history globally under `$CODEX_HOME`, so there is
        // no per-project marker directory to probe.
        false
    }

    fn exit_command(&self) -> Option<&str> {
        // Codex `Stop` is turn-scoped and there is no documented `/exit` slash
        // command; Ouija tears panes down via respawn/kill instead.
        None
    }

    fn install(&self) -> anyhow::Result<()> {
        // Bootstrap Codex hook integration: write ouija hook scripts + merge
        // hooks.json under CODEX_HOME (~/.codex). Skip silently if Codex isn't
        // set up on this host (no resolvable home), matching the other backends'
        // best-effort install semantics.
        match resolve_codex_home() {
            Some(home) => install_to(&home),
            None => Ok(()),
        }
    }

    // is_available: uses the timeout-aware default so the npx/node `codex`
    // wrapper's slow `--version` cannot stall startup or registration.

    fn description_file_priority(&self) -> &[&str] {
        // Codex reads AGENTS.md as its project guidance file.
        &["AGENTS.md", "README.md"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ResumeOpts, StartOpts, WorktreeMode};

    fn backend() -> Codex {
        Codex
    }

    fn start_opts(dir: &str) -> StartOpts {
        StartOpts {
            project_dir: dir.to_string(),
            worktree: None,
            model: None,
            effort: None,
            permission_mode: None,
            codex_home: None,
        }
    }

    fn resume_opts(dir: &str, session_id: Option<&str>) -> ResumeOpts {
        ResumeOpts {
            project_dir: dir.to_string(),
            session_id: session_id.map(String::from),
            worktree: None,
            model: None,
            effort: None,
            permission_mode: None,
            codex_home: None,
        }
    }

    #[test]
    fn identity_and_delivery() {
        let b = backend();
        assert_eq!(b.name(), "codex-cli");
        assert_eq!(b.cli_name(), "codex");
        assert_eq!(b.process_names(), &["codex"]);
        assert!(matches!(b.delivery_mode(), DeliveryMode::TuiInjection));
        assert_eq!(b.config_dir_name(), ".codex");
    }

    #[test]
    fn start_command_basic() {
        let cmd = backend().build_start_command(&start_opts("/home/user/myproject"));
        assert_eq!(
            cmd,
            "cd '/home/user/myproject' && codex --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}'"
        );
    }

    #[test]
    fn session_start_hook_flags_keep_credential_out_of_argv_and_toml() {
        let home = tempfile::tempdir().unwrap();
        let credential_file = stage_launch_credential(home.path(), "one-time-proof").unwrap();
        let flags = format_session_start_hook_flags(
            &home.path().to_string_lossy(),
            "public-launch",
            &credential_file,
        );

        assert!(
            flags.contains("codex-register.sh"),
            "hook command must invoke the installed Codex registration hook: {flags}"
        );
        assert!(
            flags.contains("public-launch"),
            "hook command must carry the public launch id: {flags}"
        );
        assert!(
            !flags.contains("one-time-proof"),
            "the launch credential must never appear in generated Codex argv or TOML: {flags}"
        );
        assert!(
            flags.contains("--launch-credential-file"),
            "hook command must receive a credential-file path, not a credential value: {flags}"
        );
        assert!(
            flags.contains(credential_file.to_string_lossy().as_ref()),
            "hook command must carry the private credential-file path: {flags}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(launch_credential_dir(home.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&credential_file)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(
            flags.contains("hooks.SessionStart="),
            "hook must be installed through a session-flags override: {flags}"
        );
        assert!(
            flags.contains(
                "hooks.state.\"<session-flags>/config.toml:session_start:0:0\".trusted_hash="
            ),
            "session-flags hook needs its exact trusted hash: {flags}"
        );
        assert!(
            !flags.contains("CODEX_THREAD_ID"),
            "native identity must stay inside the Codex adapter/hook payload: {flags}"
        );
    }

    #[test]
    fn start_command_with_model() {
        let cmd = backend().build_start_command(&StartOpts {
            model: Some("gpt-5.5".into()),
            ..start_opts("/home/user/myproject")
        });
        assert_eq!(
            cmd,
            "cd '/home/user/myproject' && codex --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}' --model 'gpt-5.5'"
        );
    }

    #[test]
    fn start_command_with_codex_home() {
        let cmd = backend().build_start_command(&StartOpts {
            model: Some("gemini-2.5-pro".into()),
            codex_home: Some("/home/user/.cache/codex-gemini".into()),
            ..start_opts("/home/user/myproject")
        });
        assert_eq!(
            cmd,
            "cd '/home/user/myproject' && CODEX_HOME='/home/user/.cache/codex-gemini' codex --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}' --model 'gemini-2.5-pro'"
        );
    }

    #[test]
    fn start_command_ignores_worktree_and_maps_effort_to_config() {
        // Codex has no verified --worktree or --effort flag. Ouija still maps
        // effort through Codex config instead of dropping it.
        let cmd = backend().build_start_command(&StartOpts {
            worktree: Some(WorktreeMode::Named("feature-x".to_string())),
            effort: Some("high".into()),
            ..start_opts("/home/user/myproject")
        });
        assert!(
            !cmd.contains("--worktree"),
            "must not emit --worktree: {cmd}"
        );
        assert!(!cmd.contains("--effort"), "must not emit --effort: {cmd}");
        assert!(
            !cmd.contains("feature-x"),
            "must not emit worktree name: {cmd}"
        );
        assert!(
            cmd.contains("-c 'model_reasoning_effort=\"high\"'"),
            "must emit effort as Codex config: {cmd}"
        );
    }

    #[test]
    fn start_command_with_model_and_effort() {
        let cmd = backend().build_start_command(&StartOpts {
            model: Some("gpt-5.5".into()),
            effort: Some("low".into()),
            ..start_opts("/home/user/myproject")
        });
        assert_eq!(
            cmd,
            "cd '/home/user/myproject' && codex --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}' -c 'model_reasoning_effort=\"low\"' --model 'gpt-5.5'"
        );
    }

    #[test]
    fn start_and_resume_use_full_power_worker_mode() {
        // Ouija runs Codex as a full-power local worker, matching Claude Code's
        // bypassPermissions posture. The chosen Ouija/Hub worktree is cwd/scope,
        // not isolation; future deployments can supply an external runner sandbox
        // around the process (#1445).
        let yolo = "--dangerously-bypass-approvals-and-sandbox";
        let start = backend().build_start_command(&start_opts("/home/user/myproject"));
        assert!(
            start.contains(yolo),
            "start must bypass approvals/sandbox: {start}"
        );
        assert!(
            start.contains("-c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}'"),
            "start must pre-trust the Codex trust root: {start}"
        );
        let resume = backend()
            .build_resume_command(&resume_opts("/home/user/myproject", None))
            .unwrap();
        assert!(
            resume.contains(yolo),
            "resume must bypass approvals/sandbox: {resume}"
        );
        assert!(
            resume.contains("-c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}'"),
            "resume must pre-trust the Codex trust root: {resume}"
        );
        assert!(!start.contains("--sandbox workspace-write"), "{start}");
        assert!(
            !start.contains("sandbox_workspace_write.network_access"),
            "{start}"
        );
    }

    #[test]
    fn trust_override_survives_spawned_worker_options() {
        // This mirrors the command shape Ouija uses for unattended Codex workers:
        // an isolated CODEX_HOME/model route, reasoning effort override, and a
        // prompt appended by the caller. The trust override must remain present
        // so the TUI never blocks on Codex's first-run project trust dialog.
        let expected_trust = " -c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}'";
        let start = backend().build_start_command(&StartOpts {
            model: Some("gemini-2.5-pro".into()),
            effort: Some("low".into()),
            codex_home: Some("/home/user/.cache/codex-gemini".into()),
            ..start_opts("/home/user/myproject")
        });
        assert!(
            start.contains(expected_trust),
            "spawn start must pre-trust the Codex project root: {start}"
        );
        assert_eq!(
            start.matches("trust_level=\"trusted\"").count(),
            1,
            "spawn start must carry one trust override: {start}"
        );

        let resume = backend()
            .build_resume_command(&ResumeOpts {
                model: Some("gemini-2.5-pro".into()),
                effort: Some("low".into()),
                codex_home: Some("/home/user/.cache/codex-gemini".into()),
                ..resume_opts("/home/user/myproject", Some("abc-123"))
            })
            .unwrap();
        assert!(
            resume.contains(expected_trust),
            "spawn resume must pre-trust the Codex project root: {resume}"
        );
        assert_eq!(
            resume.matches("trust_level=\"trusted\"").count(),
            1,
            "spawn resume must carry one trust override: {resume}"
        );
    }

    #[test]
    fn resume_command_without_session_id_uses_last() {
        let cmd = backend().build_resume_command(&resume_opts("/home/user/myproject", None));
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && codex resume --last --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}'"
                    .to_string()
            )
        );
    }

    #[test]
    fn resume_command_with_session_id() {
        let cmd =
            backend().build_resume_command(&resume_opts("/home/user/myproject", Some("abc-123")));
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && codex resume 'abc-123' --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}'"
                    .to_string()
            )
        );
    }

    #[test]
    fn resume_command_with_model() {
        let cmd = backend().build_resume_command(&ResumeOpts {
            model: Some("gpt-5.5".into()),
            ..resume_opts("/home/user/myproject", None)
        });
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && codex resume --last --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}' --model 'gpt-5.5'"
                    .to_string()
            )
        );
    }

    #[test]
    fn resume_command_with_effort() {
        let cmd = backend().build_resume_command(&ResumeOpts {
            effort: Some("xhigh".into()),
            ..resume_opts("/home/user/myproject", Some("abc-123"))
        });
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && codex resume 'abc-123' --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}' -c 'model_reasoning_effort=\"xhigh\"'"
                    .to_string()
            )
        );
    }

    #[test]
    fn resume_command_with_codex_home() {
        let cmd = backend().build_resume_command(&ResumeOpts {
            codex_home: Some("/home/user/.cache/codex-gemini".into()),
            ..resume_opts("/home/user/myproject", Some("abc-123"))
        });
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && CODEX_HOME='/home/user/.cache/codex-gemini' codex resume 'abc-123' --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c 'projects={\"/home/user/myproject\"={trust_level=\"trusted\"}}'"
                    .to_string()
            )
        );
    }

    #[test]
    fn resume_command_ignores_worktree() {
        let cmd = backend().build_resume_command(&ResumeOpts {
            worktree: Some(WorktreeMode::Disposable),
            ..resume_opts("/home/user/myproject", Some("abc-123"))
        });
        let cmd = cmd.unwrap();
        assert!(
            !cmd.contains("--worktree"),
            "must not emit --worktree: {cmd}"
        );
    }

    #[test]
    fn format_model_flag_drops_empty() {
        assert_eq!(format_model_flag(None), "");
        assert_eq!(format_model_flag(Some("")), "");
        assert_eq!(format_model_flag(Some("   ")), "");
        assert_eq!(format_model_flag(Some("gpt-5.5")), " --model 'gpt-5.5'");
    }

    #[test]
    fn format_reasoning_effort_config_flag_drops_empty_and_escapes() {
        assert_eq!(format_reasoning_effort_config_flag(None), "");
        assert_eq!(format_reasoning_effort_config_flag(Some("")), "");
        assert_eq!(format_reasoning_effort_config_flag(Some("   ")), "");
        assert_eq!(
            format_reasoning_effort_config_flag(Some("medium")),
            " -c 'model_reasoning_effort=\"medium\"'"
        );
        assert_eq!(
            format_reasoning_effort_config_flag(Some("hi\"there")),
            " -c 'model_reasoning_effort=\"hi\\\"there\"'"
        );
    }

    #[test]
    fn codex_home_prefix_drops_empty() {
        assert_eq!(format_codex_home_prefix(None), "");
        assert_eq!(format_codex_home_prefix(Some("")), "");
        assert_eq!(format_codex_home_prefix(Some("   ")), "");
    }

    #[test]
    fn trust_config_flag_uses_projects_table_form() {
        assert_eq!(
            format_trust_config_flag("/tmp/codex-trust-test-main"),
            " -c 'projects={\"/tmp/codex-trust-test-main\"={trust_level=\"trusted\"}}'"
        );
    }

    #[test]
    fn trust_config_flag_escapes_toml_and_shell() {
        assert_eq!(
            format_trust_config_flag("/tmp/quote'and\"back\\slash"),
            " -c 'projects={\"/tmp/quote'\\''and\\\"back\\\\slash\"={trust_level=\"trusted\"}}'"
        );
    }

    #[test]
    fn trust_root_uses_common_repo_parent_for_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let linked = tmp.path().join("linked");

        let output = std::process::Command::new("git")
            .args(["init"])
            .arg(&main)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::write(main.join("README.md"), "root\n").unwrap();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["add", "README.md"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["commit", "-m", "init"])
            .env("GIT_AUTHOR_NAME", "Ouija Test")
            .env("GIT_AUTHOR_EMAIL", "ouija@example.test")
            .env("GIT_COMMITTER_NAME", "Ouija Test")
            .env("GIT_COMMITTER_EMAIL", "ouija@example.test")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["worktree", "add", "-b", "linked-test"])
            .arg(&linked)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        assert_eq!(
            codex_trust_root(linked.to_str().unwrap()),
            main.canonicalize().unwrap()
        );
    }

    fn write_session_meta(path: &Path, cwd: &str, timestamp: &str, session_id: &str) {
        // First line mirrors the real Codex rollout `session_meta` record; a
        // second line ensures only the first is read.
        let meta = serde_json::json!({
            "type": "session_meta",
            "payload": { "session_id": session_id, "cwd": cwd, "timestamp": timestamp }
        });
        std::fs::write(path, format!("{meta}\n{{\"type\":\"event\"}}\n")).unwrap();
    }

    #[test]
    fn latest_session_id_for_cwd_picks_most_recent_match() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026/07/05");
        std::fs::create_dir_all(&day).unwrap();
        write_session_meta(
            &day.join("a.jsonl"),
            "/proj",
            "2026-07-05T14:44:15.000Z",
            "uuid-old",
        );
        write_session_meta(
            &day.join("b.jsonl"),
            "/proj",
            "2026-07-05T15:00:00.000Z",
            "uuid-new",
        );
        write_session_meta(
            &day.join("c.jsonl"),
            "/other",
            "2026-07-05T16:00:00.000Z",
            "uuid-other",
        );
        assert_eq!(
            latest_session_id_for_cwd(tmp.path(), "/proj"),
            Some("uuid-new".to_string())
        );
        // No session recorded for this cwd → None (caller falls back to --last).
        assert_eq!(latest_session_id_for_cwd(tmp.path(), "/nope"), None);
    }

    #[test]
    fn latest_session_id_for_cwd_none_for_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            latest_session_id_for_cwd(&tmp.path().join("sessions"), "/proj"),
            None
        );
    }

    #[test]
    fn latest_session_id_for_cwd_skips_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026/07/05");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("bad.jsonl"), "not json at all\n").unwrap();
        write_session_meta(
            &day.join("good.jsonl"),
            "/proj",
            "2026-07-05T15:00:00.000Z",
            "uuid-good",
        );
        assert_eq!(
            latest_session_id_for_cwd(tmp.path(), "/proj"),
            Some("uuid-good".to_string())
        );
    }

    #[test]
    fn tui_ready_pattern_is_prompt_glyph() {
        assert_eq!(backend().tui_ready_pattern(), Some("\u{203A}"));
    }

    #[test]
    fn inject_config_matches_live_verified_values() {
        // Locks the values verified against a live Codex TUI pane (followup #692).
        // If injection tuning ever needs to change, re-verify against a live pane
        // and update both this test and the doc comment together.
        let cfg = backend().inject_config();
        assert_eq!(cfg.paste_settle_ms, 300);
        assert!(cfg.use_inner_bracketed_paste);
        assert_eq!(cfg.startup_inject_delay_secs, 5);
    }

    #[test]
    fn has_project_history_always_false() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".codex")).unwrap();
        // Even with a local .codex dir, history is global — no per-project marker.
        assert!(!backend().has_project_history(tmp.path()));
    }

    // --- Hook install (chunk 3) ---

    #[test]
    fn register_script_wraps_output_as_session_start_context() {
        let s = embedded::SCRIPT_REGISTER;
        // TMUX_PANE can be inherited from Codex's shared app-server process.
        // Confirm the targeted pane is actually in the payload's project before
        // the hook sends a claim that could mutate another Ouija session.
        assert!(
            s.contains("tmux display-message -p -t \"$PANE\" '#{pane_current_path}'"),
            "register script must query the targeted pane cwd: {s}"
        );
        assert!(
            s.contains("[ \"$PANE_CWD\" != \"$CWD\" ] && exit 0"),
            "register script must skip a mismatched pane claim: {s}"
        );
        // Registers via the shared session-start endpoint.
        assert!(s.contains("/api/hooks/session-start"), "{s}");
        // Supplies the backend-native identity through the generic hook field.
        assert!(s.contains(".session_id"), "{s}");
        assert!(s.contains("backend_session_id"), "{s}");
        // The installed adapter identifies itself and forwards the managed
        // launch identity stamped into the pane by Ouija.
        assert!(s.contains("--arg adapter \"codex-cli\""), "{s}");
        assert!(s.contains("launch_session_id"), "{s}");
        assert!(s.contains("${OUIJA_SESSION_ID:-}"), "{s}");
        assert!(s.contains("launch_credential"), "{s}");
        assert!(s.contains("${OUIJA_SESSION_START_CREDENTIAL:-}"), "{s}");
        assert!(
            s.contains("--launch-session-id") && s.contains("--launch-credential-file"),
            "the per-launch SessionFlags hook must receive its proof by private file: {s}"
        );
        assert!(
            s.contains("mv -- \"$LAUNCH_CREDENTIAL_FILE\"")
                && s.contains("rm -f -- \"$CLAIMED_CREDENTIAL_FILE\""),
            "the hook must atomically claim then remove the one-shot credential file: {s}"
        );
        assert!(
            !s.contains("[ -z \"$PANE\" ] && exit 0"),
            "a shared app-server has no pane; the hook must submit a paneless proof claim: {s}"
        );
        // Wraps the daemon's `.output` into Codex additionalContext, keyed to the
        // SessionStart event so the TUI surfaces mesh instructions.
        assert!(s.contains("hookSpecificOutput"), "{s}");
        assert!(s.contains("additionalContext"), "{s}");
        assert!(s.contains("SessionStart"), "{s}");
        // Must never unregister on session start.
        assert!(!s.contains("session-end"), "{s}");
    }

    #[test]
    fn prompt_submit_script_signals_activity() {
        let s = embedded::SCRIPT_PROMPT_SUBMIT;
        assert!(s.contains("/api/hooks/prompt-submit"), "{s}");
        assert!(s.contains("UserPromptSubmit"), "{s}");
    }

    #[test]
    fn stop_script_is_turn_scoped_and_never_unregisters() {
        let s = embedded::SCRIPT_STOP;
        // Pings the turn-stop endpoint...
        assert!(s.contains("/api/hooks/stop"), "{s}");
        // ...returns {"continue":true} so Codex proceeds...
        assert!(s.contains(r#"{"continue":true}"#), "{s}");
        // ...and must NOT unregister (Codex Stop fires every turn).
        assert!(!s.contains("session-end"), "{s}");
    }

    #[test]
    fn merge_hooks_json_fresh_registers_all_three_events() {
        let hooks_dir = Path::new("/home/user/.codex/ouija-hooks");
        let merged = merge_hooks_json(None, hooks_dir).unwrap();
        let hooks = &merged["hooks"];
        for (event, script) in [
            ("SessionStart", "codex-register.sh"),
            ("UserPromptSubmit", "codex-prompt-submit.sh"),
            ("Stop", "codex-stop.sh"),
        ] {
            let cmd = hooks[event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("missing command for {event}: {merged}"));
            assert_eq!(cmd, format!("/home/user/.codex/ouija-hooks/{script}"));
            assert_eq!(hooks[event][0]["hooks"][0]["type"], "command");
        }
    }

    #[test]
    fn merge_hooks_json_is_idempotent() {
        let hooks_dir = Path::new("/home/user/.codex/ouija-hooks");
        let once = merge_hooks_json(None, hooks_dir).unwrap();
        let twice = merge_hooks_json(Some(&once.to_string()), hooks_dir).unwrap();
        assert_eq!(once, twice, "second install must not duplicate ouija hooks");
        // Exactly one SessionStart group (no duplication).
        assert_eq!(twice["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn merge_hooks_json_preserves_user_hooks() {
        let hooks_dir = Path::new("/home/user/.codex/ouija-hooks");
        let existing = serde_json::json!({
            "hooks": {
                "PostToolUse": [
                    { "matcher": "Write", "hooks": [
                        { "type": "command", "command": "./scripts/user-thing.sh" }
                    ] }
                ],
                "SessionStart": [
                    { "hooks": [
                        { "type": "command", "command": "/opt/user/other-start.sh" }
                    ] }
                ]
            }
        })
        .to_string();
        let merged = merge_hooks_json(Some(&existing), hooks_dir).unwrap();
        // Unrelated event preserved verbatim.
        assert_eq!(
            merged["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "./scripts/user-thing.sh"
        );
        // The user's own SessionStart group survives alongside ours.
        let starts = merged["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(starts.len(), 2, "user + ouija groups: {merged}");
        assert!(
            starts
                .iter()
                .any(|g| g["hooks"][0]["command"] == "/opt/user/other-start.sh"),
            "user SessionStart hook must be preserved: {merged}"
        );
        assert!(
            starts
                .iter()
                .any(|g| g["hooks"][0]["command"]
                    == "/home/user/.codex/ouija-hooks/codex-register.sh"),
            "ouija SessionStart hook must be present: {merged}"
        );
    }

    #[test]
    fn merge_hooks_json_replaces_stale_ouija_command() {
        // A prior install wrote a different script path under ouija-hooks; a new
        // install must replace it, not accumulate a second ouija group.
        let hooks_dir = Path::new("/home/user/.codex/ouija-hooks");
        let stale = serde_json::json!({
            "hooks": {
                "Stop": [
                    { "hooks": [
                        { "type": "command",
                          "command": "/home/user/.codex/ouija-hooks/old-stop.sh" }
                    ] }
                ]
            }
        })
        .to_string();
        let merged = merge_hooks_json(Some(&stale), hooks_dir).unwrap();
        let stops = merged["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stops.len(),
            1,
            "stale ouija group must be replaced: {merged}"
        );
        assert_eq!(
            stops[0]["hooks"][0]["command"],
            "/home/user/.codex/ouija-hooks/codex-stop.sh"
        );
    }

    #[test]
    fn install_to_writes_scripts_and_hooks_json() {
        let home = tempfile::tempdir().unwrap();
        install_to(home.path()).unwrap();

        let hooks_dir = home.path().join("ouija-hooks");
        for script in [
            "codex-register.sh",
            "codex-prompt-submit.sh",
            "codex-stop.sh",
        ] {
            let p = hooks_dir.join(script);
            assert!(p.is_file(), "missing script {script}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&p).unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111, "{script} must be executable");
            }
        }
        assert_eq!(
            std::fs::read_to_string(hooks_dir.join("codex-register.sh")).unwrap(),
            embedded::SCRIPT_REGISTER,
            "the installed register hook must retain its pane-cwd safety check"
        );

        let hooks_json = std::fs::read_to_string(home.path().join("hooks.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&hooks_json).unwrap();
        assert_eq!(
            parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            hooks_dir
                .join("codex-register.sh")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn install_to_is_idempotent_on_disk() {
        let home = tempfile::tempdir().unwrap();
        install_to(home.path()).unwrap();
        install_to(home.path()).unwrap();
        let hooks_json = std::fs::read_to_string(home.path().join("hooks.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&hooks_json).unwrap();
        assert_eq!(
            parsed["hooks"]["SessionStart"].as_array().unwrap().len(),
            1,
            "re-install must not duplicate hooks: {hooks_json}"
        );
    }

    #[test]
    fn merge_hooks_json_none_when_existing_unparseable() {
        // An existing-but-unparseable hooks.json must signal "do not touch",
        // never be silently treated as empty and overwritten (finding f3).
        let hooks_dir = Path::new("/home/user/.codex/ouija-hooks");
        assert!(merge_hooks_json(Some("{ not valid json"), hooks_dir).is_none());
        // A present-but-empty string is also unparseable → leave untouched.
        assert!(merge_hooks_json(Some(""), hooks_dir).is_none());
        // No existing file still installs fresh.
        assert!(merge_hooks_json(None, hooks_dir).is_some());
    }

    #[test]
    fn install_to_writes_ouija_skill() {
        let home = tempfile::tempdir().unwrap();
        install_to(home.path()).unwrap();
        let skill = home.path().join("skills/ouija/SKILL.md");
        assert!(skill.is_file(), "ouija SKILL.md must be installed");
        assert_eq!(std::fs::read_to_string(&skill).unwrap(), embedded::SKILL_MD);
    }

    #[test]
    fn install_skill_is_idempotent_and_preserves_unrelated_skills() {
        let home = tempfile::tempdir().unwrap();
        // A pre-existing unrelated user skill must survive install.
        let other = home.path().join("skills/my-skill");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("SKILL.md"), "user skill body").unwrap();

        install_skill_to(home.path()).unwrap();
        install_skill_to(home.path()).unwrap(); // re-install must not error or duplicate

        // Ouija skill present with expected content.
        assert_eq!(
            std::fs::read_to_string(home.path().join("skills/ouija/SKILL.md")).unwrap(),
            embedded::SKILL_MD
        );
        // Unrelated skill untouched.
        assert_eq!(
            std::fs::read_to_string(other.join("SKILL.md")).unwrap(),
            "user skill body"
        );
    }

    #[test]
    fn install_to_leaves_unparseable_hooks_json_untouched() {
        let home = tempfile::tempdir().unwrap();
        let hooks_path = home.path().join("hooks.json");
        let garbage = "{ this is not valid json — user hand-edit in progress";
        std::fs::write(&hooks_path, garbage).unwrap();

        install_to(home.path()).unwrap();

        // The user's file must survive verbatim — never silently discarded.
        assert_eq!(std::fs::read_to_string(&hooks_path).unwrap(), garbage);
        // Scripts are still written (harmless); only hooks.json is left alone.
        assert!(home.path().join("ouija-hooks/codex-register.sh").is_file());
    }
}
