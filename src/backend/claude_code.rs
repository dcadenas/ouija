use std::path::Path;

use super::{CodingAssistant, DeliveryMode, InjectConfig, ResumeOpts, StartOpts};

#[derive(Debug)]
pub struct ClaudeCode;

/// Pre-trust a workspace directory so Claude Code skips the trust dialog.
///
/// Writes `hasTrustDialogAccepted: true` into `~/.claude.json` for the given
/// directory, and also ensures the `~/.claude/projects/<escaped>/` session
/// data directory exists.
pub fn pre_trust_workspace(dir: &str) {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };

    // Ensure session data directory exists
    let escaped = dir.replace('/', "-");
    let _ = std::fs::create_dir_all(format!("{home}/.claude/projects/{escaped}"));

    // Write trust entry to ~/.claude.json
    let claude_json_path = format!("{home}/.claude.json");
    let mut data: serde_json::Value = std::fs::read_to_string(&claude_json_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let projects = data.as_object_mut().and_then(|obj| {
        obj.entry("projects")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
    });

    if let Some(projects) = projects {
        let entry = projects.entry(dir).or_insert_with(|| serde_json::json!({}));
        if let Some(obj) = entry.as_object_mut() {
            if obj.get("hasTrustDialogAccepted") == Some(&serde_json::Value::Bool(true)) {
                return; // already trusted
            }
            obj.insert(
                "hasTrustDialogAccepted".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        if let Ok(json) = serde_json::to_string_pretty(&data) {
            let _ = std::fs::write(&claude_json_path, json);
        }
    }
}

// --- Embedded plugin files ---
// These are compiled into the binary so `ouija start-server` can bootstrap the Claude
// Code plugin without needing the source repo on disk.

mod embedded {
    pub const HOOKS_JSON: &str = include_str!("../../hooks/hooks.json");

    pub const SCRIPT_BLOCK_INTERACTIVE: &str =
        include_str!("../../scripts/block-interactive-prompts.sh");
    pub const SCRIPT_CHECK_PENDING: &str = include_str!("../../scripts/check-pending-replies.sh");
    pub const SCRIPT_HOOK_INCARNATION: &str = include_str!("../../scripts/hook-incarnation.sh");
    pub const SCRIPT_PROMPT_SUBMIT: &str = include_str!("../../scripts/ouija-prompt-submit.sh");
    pub const SCRIPT_REGISTER: &str = include_str!("../../scripts/ouija-register.sh");
    pub const SCRIPT_STATUSLINE: &str = include_str!("../../scripts/ouija-statusline.sh");
    pub const SCRIPT_POST_COMPACT: &str = include_str!("../../scripts/post-compact.sh");
    pub const SCRIPT_TOOL_ACTIVITY: &str = include_str!("../../scripts/ouija-tool-activity.sh");
    pub const SCRIPT_UNREGISTER: &str = include_str!("../../scripts/ouija-unregister.sh");

    pub const SKILLS_PEER_TRUST: &str = include_str!("../../skills/ouija/SKILL.md");
    pub const PLUGIN_JSON: &str = include_str!("../../.claude-plugin/plugin.json");
    pub const MARKETPLACE_JSON: &str = include_str!("../../.claude-plugin/marketplace.json");
}

/// Compare the previously-stamped plugin version against the current daemon
/// version. Returns `Some(previous)` when a mismatch warning should be
/// printed, `None` when the versions match or the previous stamp is absent
/// (fresh install). An unreadable or empty stamp is treated as absent.
///
/// This is the operator-facing replacement for the old session-start LLM
/// context injection: if a long-running coding session was spawned before a
/// daemon upgrade, its cached hook scripts may still predate the running
/// daemon until the session is restarted.
fn version_mismatch_to_report(previous: Option<&str>, current: &str) -> Option<String> {
    let prev = previous?.trim();
    if prev.is_empty() || prev == current {
        None
    } else {
        Some(prev.to_string())
    }
}

/// Print a stderr warning when the plugin cache's old `.version` differs
/// from the daemon binary's version. Silent otherwise. Called from
/// `ensure_plugin_installed` and `refresh_plugin_cache` right before they
/// overwrite the stamp.
fn warn_if_plugin_version_skew(cache_dir: &std::path::Path, current: &str) {
    let prev = std::fs::read_to_string(cache_dir.join(".version")).ok();
    if let Some(old) = version_mismatch_to_report(prev.as_deref(), current) {
        eprintln!(
            "warning: ouija plugin cache was previously stamped {old}, daemon is {current} — \
             restart any running coding sessions so they pick up the new hook scripts."
        );
    }
}

/// Write all embedded plugin files to the given cache directory.
fn write_embedded_plugin_files(cache_dir: &std::path::Path) {
    let files: &[(&str, &str)] = &[
        ("hooks/hooks.json", embedded::HOOKS_JSON),
        (
            "scripts/block-interactive-prompts.sh",
            embedded::SCRIPT_BLOCK_INTERACTIVE,
        ),
        (
            "scripts/check-pending-replies.sh",
            embedded::SCRIPT_CHECK_PENDING,
        ),
        (
            "scripts/hook-incarnation.sh",
            embedded::SCRIPT_HOOK_INCARNATION,
        ),
        (
            "scripts/ouija-prompt-submit.sh",
            embedded::SCRIPT_PROMPT_SUBMIT,
        ),
        ("scripts/ouija-register.sh", embedded::SCRIPT_REGISTER),
        ("scripts/ouija-statusline.sh", embedded::SCRIPT_STATUSLINE),
        (
            "scripts/ouija-tool-activity.sh",
            embedded::SCRIPT_TOOL_ACTIVITY,
        ),
        ("scripts/ouija-unregister.sh", embedded::SCRIPT_UNREGISTER),
        ("scripts/post-compact.sh", embedded::SCRIPT_POST_COMPACT),
        ("skills/ouija/SKILL.md", embedded::SKILLS_PEER_TRUST),
        (".claude-plugin/plugin.json", embedded::PLUGIN_JSON),
        (
            ".claude-plugin/marketplace.json",
            embedded::MARKETPLACE_JSON,
        ),
    ];

    for (path, content) in files {
        let dest = cache_dir.join(path);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&dest, content);
    }

    // Make scripts executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(entries) = std::fs::read_dir(cache_dir.join("scripts")) {
            for entry in entries.flatten() {
                let _ =
                    std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o755));
            }
        }
    }
}

fn sync_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            sync_dir(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Try to sync plugin files from the local source directory. Returns true if
/// a source dir was found and synced.
fn try_sync_from_source(home: &std::path::Path, cache_dir: &std::path::Path) -> bool {
    let settings_path = home.join(".claude/settings.json");
    let settings_str = match std::fs::read_to_string(&settings_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let settings: serde_json::Value = match serde_json::from_str(&settings_str) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let source_dir = match settings
        .pointer("/extraKnownMarketplaces/ouija/source/path")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
    {
        Some(d) if d.exists() => d,
        _ => return false,
    };

    for dir in &["scripts", "hooks", "skills"] {
        let src = source_dir.join(dir);
        let dst = cache_dir.join(dir);
        if src.is_dir() {
            if let Err(e) = sync_dir(&src, &dst) {
                eprintln!("warning: failed to sync plugin {dir}: {e}");
            }
        }
    }

    let src = source_dir.join(".claude-plugin");
    let dst = cache_dir.join(".claude-plugin");
    if src.is_dir() {
        if let Err(e) = sync_dir(&src, &dst) {
            eprintln!("warning: failed to sync plugin .claude-plugin: {e}");
        }
    }

    true
}

/// Ensure the Claude Code plugin is installed. Called on every `ouija start-server`.
/// If the plugin cache already exists, just stamps the version. If not, writes
/// all embedded files and registers in installed_plugins.json / settings.json.
fn ensure_plugin_installed() {
    let home = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h),
        Err(_) => return,
    };

    let claude_dir = home.join(".claude");
    if !claude_dir.exists() {
        // Claude Code not installed — skip silently
        return;
    }

    let version = env!("CARGO_PKG_VERSION");
    let cache_dir = claude_dir.join("plugins/cache/ouija/ouija/0.1.0");

    let needs_full_install = !cache_dir.exists();
    if needs_full_install {
        println!("installing Claude Code plugin...");
    }

    write_embedded_plugin_files(&cache_dir);

    // Warn the operator if the previously-stamped plugin version differs
    // from the running daemon, BEFORE we overwrite .version.
    warn_if_plugin_version_skew(&cache_dir, version);

    // Stamp version
    let _ = std::fs::write(cache_dir.join(".version"), version);

    // Ensure extraKnownMarketplaces and statusLine exist (may be missing on upgrades)
    {
        let settings_path = claude_dir.join("settings.json");
        let mut settings: serde_json::Value = std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let mut changed = false;
        if let Some(obj) = settings.as_object_mut() {
            let mkts = obj
                .entry("extraKnownMarketplaces")
                .or_insert_with(|| serde_json::json!({}));
            if mkts.get("ouija").is_none() {
                mkts["ouija"] = serde_json::json!({
                    "source": {
                        "source": "directory",
                        "path": cache_dir.to_string_lossy()
                    }
                });
                changed = true;
                println!("registered ouija in extraKnownMarketplaces");
            }

            if obj.get("statusLine").is_none() {
                let script = cache_dir.join("scripts/ouija-statusline.sh");
                obj.insert(
                    "statusLine".to_string(),
                    serde_json::json!({
                        "type": "command",
                        "command": script.to_string_lossy()
                    }),
                );
                changed = true;
                println!("configured ouija status line");
            }
        }
        if changed {
            let _ = std::fs::write(
                &settings_path,
                serde_json::to_string_pretty(&settings).unwrap(),
            );
        }
    }

    if !needs_full_install {
        return;
    }

    // --- First-time registration ---

    // Update installed_plugins.json
    let plugins_path = claude_dir.join("plugins/installed_plugins.json");
    let mut plugins: serde_json::Value = std::fs::read_to_string(&plugins_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| {
            serde_json::json!({
                "version": 2,
                "plugins": {}
            })
        });

    if !plugins["plugins"]
        .as_object()
        .is_some_and(|p| p.contains_key("ouija@ouija"))
    {
        let now = chrono::Utc::now().to_rfc3339();
        plugins["plugins"]["ouija@ouija"] = serde_json::json!([{
            "scope": "user",
            "installPath": cache_dir.to_string_lossy(),
            "version": "0.1.0",
            "installedAt": now,
            "lastUpdated": now,
            "isLocal": false
        }]);
        let _ = std::fs::write(
            &plugins_path,
            serde_json::to_string_pretty(&plugins).unwrap(),
        );
    }

    // Update settings.json — enable the plugin
    let settings_path = claude_dir.join("settings.json");
    let mut settings: serde_json::Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let mut changed = false;
    if let Some(obj) = settings.as_object_mut() {
        let enabled = obj
            .entry("enabledPlugins")
            .or_insert_with(|| serde_json::json!({}));
        if enabled.get("ouija@ouija").is_none() {
            enabled["ouija@ouija"] = serde_json::Value::Bool(true);
            changed = true;
        }
    }

    if changed {
        let _ = std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        );
    }

    println!("Claude Code plugin installed. Restart Claude Code sessions to activate.");
}

/// Refresh the Claude Code plugin cache from the source directory.
///
/// Tries the source directory first (for local dev), falls back to embedded
/// files (for production installs).
pub fn refresh_plugin_cache(version: &str) {
    let home = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h),
        Err(_) => return,
    };

    let cache_base = home.join(".claude/plugins/cache/ouija/ouija");
    let cache_dir = match std::fs::read_dir(&cache_base)
        .ok()
        .and_then(|mut entries| entries.next())
        .and_then(|e| e.ok())
    {
        Some(entry) => entry.path(),
        None => {
            // No cache dir yet — run full install with embedded files
            ensure_plugin_installed();
            return;
        }
    };

    // Try source directory first (local dev workflow)
    let source_synced = try_sync_from_source(&home, &cache_dir);

    if !source_synced {
        // Fall back to embedded files (production install via cargo)
        write_embedded_plugin_files(&cache_dir);
    }

    // Warn the operator before overwriting if the previous stamp differs.
    warn_if_plugin_version_skew(&cache_dir, version);

    // Stamp version so the next daemon start can detect plugin/daemon mismatch.
    let _ = std::fs::write(cache_dir.join(".version"), version);

    println!("plugin cache refreshed");
}

/// Render ` --model <X> --effort <Y>` fragments for the claude CLI.
///
/// Returns an empty string when both are `None`. Values are shell-escaped so
/// special characters embed safely inside the surrounding `format!`-built
/// shell command. Each returned flag is prefixed with a leading space so the
/// fragment can be concatenated directly onto the command string.
///
/// Empty / whitespace-only values are treated as absent as a defensive guard
/// against an empty string slipping past the API boundary. Producing
/// `claude --model ''` would fail at runtime on the CLI anyway; omitting the
/// flag is the safer default.
fn format_model_effort_flags(model: Option<&str>, effort: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(m) = model
        && !m.trim().is_empty()
    {
        out.push_str(" --model ");
        out.push_str(&crate::scheduler::shell_escape(m));
    }
    if let Some(e) = effort
        && !e.trim().is_empty()
    {
        out.push_str(" --effort ");
        out.push_str(&crate::scheduler::shell_escape(e));
    }
    out
}

fn format_permission_mode_flag(permission_mode: Option<&str>) -> String {
    match permission_mode
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
    {
        Some(mode) => format!(
            " --permission-mode {}",
            crate::scheduler::shell_escape(mode)
        ),
        None => String::new(),
    }
}

impl CodingAssistant for ClaudeCode {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn cli_name(&self) -> &str {
        "claude"
    }

    fn process_names(&self) -> &[&str] {
        &["claude"]
    }

    fn delivery_mode(&self) -> DeliveryMode {
        DeliveryMode::TuiInjection
    }

    fn build_start_command(&self, opts: &StartOpts) -> String {
        let escaped_dir = crate::scheduler::shell_escape(&opts.project_dir);
        let permission_mode = format_permission_mode_flag(opts.permission_mode.as_deref());
        let model_effort = format_model_effort_flags(opts.model.as_deref(), opts.effort.as_deref());
        match &opts.worktree {
            None => format!("cd {escaped_dir} && claude{permission_mode}{model_effort}"),
            Some(super::WorktreeMode::Disposable) => {
                format!("cd {escaped_dir} && claude{permission_mode}{model_effort} --worktree")
            }
            Some(super::WorktreeMode::Named(name)) => {
                let escaped_name = crate::scheduler::shell_escape(name);
                format!(
                    "cd {escaped_dir} && claude{permission_mode}{model_effort} --worktree {escaped_name}"
                )
            }
        }
    }

    fn build_resume_command(&self, opts: &ResumeOpts) -> Option<String> {
        let escaped_dir = crate::scheduler::shell_escape(&opts.project_dir);
        let permission_mode = format_permission_mode_flag(opts.permission_mode.as_deref());
        let resume_flag = match &opts.session_id {
            Some(sid) => format!("--resume {}", crate::scheduler::shell_escape(sid)),
            None => "--continue".to_string(),
        };
        let model_effort = format_model_effort_flags(opts.model.as_deref(), opts.effort.as_deref());
        let cmd = match &opts.worktree {
            None => {
                format!("cd {escaped_dir} && claude{permission_mode} {resume_flag}{model_effort}")
            }
            Some(super::WorktreeMode::Disposable) => {
                format!(
                    "cd {escaped_dir} && claude{permission_mode} {resume_flag}{model_effort} --worktree"
                )
            }
            Some(super::WorktreeMode::Named(name)) => {
                let escaped_name = crate::scheduler::shell_escape(name);
                format!(
                    "cd {escaped_dir} && claude{permission_mode} {resume_flag}{model_effort} --worktree {escaped_name}"
                )
            }
        };
        Some(cmd)
    }

    fn detect_session_id(&self, project_dir: &str) -> Option<String> {
        let home = std::env::var("HOME").ok()?;
        // Claude encodes project dirs as: absolute path with / replaced by -
        // e.g. /home/daniel/code/ouija -> -home-daniel-code-ouija
        let slug = project_dir.replace('/', "-");
        let sessions_dir = std::path::PathBuf::from(&home)
            .join(".claude")
            .join("projects")
            .join(&slug);
        if !sessions_dir.is_dir() {
            return None;
        }

        // Find the most recently modified .jsonl file
        let mut newest: Option<(std::time::SystemTime, String)> = None;
        let entries = std::fs::read_dir(&sessions_dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            let stem = path.file_stem()?.to_str()?.to_string();
            if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
                newest = Some((modified, stem));
            }
        }

        let (_, session_id) = newest?;
        tracing::debug!(
            "auto-detected claude session {session_id} from {}",
            sessions_dir.display()
        );
        Some(session_id)
    }

    fn tui_ready_pattern(&self) -> Option<&str> {
        Some("\u{276F}")
    }

    fn inject_config(&self) -> InjectConfig {
        InjectConfig {
            paste_settle_ms: 300,
            use_inner_bracketed_paste: true,
            startup_inject_delay_secs: 5,
        }
    }

    fn config_dir_name(&self) -> &str {
        ".claude"
    }

    fn resolve_project_root<'a>(&self, path: &'a str) -> &'a str {
        // Strip /.claude/worktrees/<branch> suffix if present
        if let Some(idx) = path.find("/.claude/worktrees/") {
            &path[..idx]
        } else {
            path
        }
    }

    fn has_project_history(&self, dir: &Path) -> bool {
        dir.join(".claude").is_dir()
    }

    fn compact_command(&self) -> Option<&str> {
        Some("/compact")
    }

    fn exit_command(&self) -> Option<&str> {
        Some("/exit")
    }

    fn install(&self) -> anyhow::Result<()> {
        ensure_plugin_installed();
        Ok(())
    }

    // is_available: uses default impl (runs `self.cli_name() --version`)

    fn description_file_priority(&self) -> &[&str] {
        &["CLAUDE.md", "README.md"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ResumeOpts, StartOpts, WorktreeMode};
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn backend() -> ClaudeCode {
        ClaudeCode
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
    fn register_script_reports_claude_adapter_and_managed_launch_identity() {
        let script = embedded::SCRIPT_REGISTER;
        assert!(script.contains("--arg adapter \"claude-code\""), "{script}");
        assert!(script.contains("launch_session_id"), "{script}");
        assert!(script.contains("${OUIJA_SESSION_ID:-}"), "{script}");
        assert!(script.contains("OUIJA_SESSION_INCARNATION"), "{script}");
        assert!(script.contains("session_incarnation"), "{script}");
        assert!(
            embedded::SCRIPT_UNREGISTER.contains("OUIJA_SESSION_INCARNATION"),
            "{}",
            embedded::SCRIPT_UNREGISTER
        );
        assert!(
            embedded::SCRIPT_TOOL_ACTIVITY.contains("session_incarnation"),
            "{}",
            embedded::SCRIPT_TOOL_ACTIVITY
        );
        assert!(
            embedded::SCRIPT_PROMPT_SUBMIT.contains("session_incarnation"),
            "{}",
            embedded::SCRIPT_PROMPT_SUBMIT
        );
    }

    #[test]
    fn hook_scripts_keep_incarnations_bound_to_backend_threads() {
        let root = tempfile::tempdir().unwrap();
        let plugin_dir = root.path().join("plugin");
        let bin_dir = root.path().join("bin");
        let capture = root.path().join("requests.jsonl");
        let runtime_dir = root.path().join("runtime");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&runtime_dir).unwrap();
        write_embedded_plugin_files(&plugin_dir);

        let fake_curl = bin_dir.join("curl");
        std::fs::write(
            &fake_curl,
            r#"#!/bin/bash
body='{}'
url=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    -d) body="$2"; shift 2 ;;
    http://*) url="$1"; shift ;;
    *) shift ;;
  esac
done
jq -cn --arg url "$url" --argjson body "$body" '{url:$url,body:$body}' >> "$OUIJA_HOOK_CAPTURE"
if [[ "$url" == */api/hooks/session-start ]]; then
  thread=$(printf '%s' "$body" | jq -r '.backend_session_id // empty')
  case "$thread" in
    thread-old) incarnation=41 ;;
    thread-new) incarnation=42 ;;
    *) incarnation=43 ;;
  esac
  jq -cn --arg incarnation "$incarnation" \
    '{registered:"worker",session_incarnation:$incarnation,output:""}'
else
  printf '{}'
fi
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let run_hook = |script: &str, payload: &str| {
            let mut child = Command::new("bash")
                .arg(plugin_dir.join("scripts").join(script))
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        bin_dir.display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .env("HOME", root.path())
                .env("XDG_RUNTIME_DIR", &runtime_dir)
                .env("TMUX_PANE", "%42")
                .env("OUIJA_HOOK_CAPTURE", &capture)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(payload.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{script} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };

        run_hook(
            "ouija-register.sh",
            r#"{"session_id":"thread-old","cwd":"/repo"}"#,
        );
        run_hook(
            "ouija-register.sh",
            r#"{"session_id":"thread-new","cwd":"/repo"}"#,
        );
        run_hook("check-pending-replies.sh", r#"{"session_id":"thread-old"}"#);
        run_hook("ouija-prompt-submit.sh", r#"{"session_id":"thread-new"}"#);
        run_hook(
            "ouija-tool-activity.sh",
            r#"{"session_id":"thread-new","tool_name":"Bash"}"#,
        );
        run_hook("post-compact.sh", r#"{"session_id":"thread-new"}"#);
        run_hook("ouija-unregister.sh", r#"{"session_id":"thread-old"}"#);
        run_hook(
            "ouija-tool-activity.sh",
            r#"{"session_id":"thread-new","tool_name":"Bash"}"#,
        );
        run_hook("ouija-unregister.sh", r#"{"session_id":"thread-new"}"#);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::fs::read_to_string(&capture)
            .map(|contents| contents.lines().count())
            .unwrap_or_default()
            < 9
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let requests: Vec<serde_json::Value> = std::fs::read_to_string(&capture)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let hook_requests: Vec<_> = requests
            .iter()
            .filter(|request| !request["url"].as_str().unwrap().ends_with("session-start"))
            .collect();
        let expected = [
            ("/api/hooks/stop", "thread-old", "41", 1),
            ("/api/hooks/prompt-submit", "thread-new", "42", 1),
            ("/api/hooks/pre-tool-use", "thread-new", "42", 2),
            ("/api/hooks/post-compact", "thread-new", "42", 1),
            ("/api/hooks/session-end", "thread-old", "41", 1),
            ("/api/hooks/session-end", "thread-new", "42", 1),
        ];
        assert_eq!(hook_requests.len(), 7);
        for (path, thread, incarnation, expected_count) in expected {
            let actual_count = hook_requests
                .iter()
                .filter(|request| {
                    request["url"].as_str().unwrap().ends_with(path)
                        && request["body"]["backend_session_id"] == thread
                        && request["body"]["session_incarnation"] == incarnation
                })
                .count();
            assert_eq!(
                actual_count, expected_count,
                "wrong requests for {path}, backend thread {thread}, incarnation {incarnation}: {hook_requests:?}"
            );
        }
    }

    #[test]
    fn embedded_skill_teaches_opt_in_task_reminders_for_claude_code() {
        crate::backend::assert_shared_task_reminder_guidance(embedded::SKILLS_PEER_TRUST);
    }

    #[test]
    fn start_command_no_worktree() {
        let cmd = backend().build_start_command(&start_opts("/home/user/myproject"));
        assert_eq!(cmd, "cd '/home/user/myproject' && claude");
    }

    #[test]
    fn start_command_named_worktree() {
        let cmd = backend().build_start_command(&StartOpts {
            worktree: Some(WorktreeMode::Named("feature-x".to_string())),
            ..start_opts("/home/user/myproject")
        });
        assert_eq!(
            cmd,
            "cd '/home/user/myproject' && claude --worktree 'feature-x'"
        );
    }

    #[test]
    fn start_command_disposable_worktree() {
        let cmd = backend().build_start_command(&StartOpts {
            worktree: Some(WorktreeMode::Disposable),
            ..start_opts("/home/user/myproject")
        });
        assert_eq!(cmd, "cd '/home/user/myproject' && claude --worktree");
    }

    #[test]
    fn start_command_with_model() {
        let cmd = backend().build_start_command(&StartOpts {
            model: Some("sonnet".into()),
            ..start_opts("/home/user/myproject")
        });
        assert_eq!(cmd, "cd '/home/user/myproject' && claude --model 'sonnet'");
    }

    #[test]
    fn start_command_with_effort_only() {
        let cmd = backend().build_start_command(&StartOpts {
            effort: Some("max".into()),
            ..start_opts("/home/user/myproject")
        });
        assert_eq!(cmd, "cd '/home/user/myproject' && claude --effort 'max'");
    }

    #[test]
    fn start_command_with_model_and_effort() {
        let cmd = backend().build_start_command(&StartOpts {
            model: Some("opus".into()),
            effort: Some("high".into()),
            ..start_opts("/home/user/myproject")
        });
        assert_eq!(
            cmd,
            "cd '/home/user/myproject' && claude --model 'opus' --effort 'high'"
        );
    }

    #[test]
    fn start_command_with_model_effort_and_named_worktree() {
        let cmd = backend().build_start_command(&StartOpts {
            worktree: Some(WorktreeMode::Named("feature-x".to_string())),
            model: Some("sonnet".into()),
            effort: Some("max".into()),
            ..start_opts("/home/user/myproject")
        });
        assert_eq!(
            cmd,
            "cd '/home/user/myproject' && claude --model 'sonnet' --effort 'max' --worktree 'feature-x'"
        );
    }

    #[test]
    fn start_command_shell_escapes_model_with_special_chars() {
        // Unlikely in practice but proves the passthrough survives quoting.
        let cmd = backend().build_start_command(&StartOpts {
            model: Some("weird model".into()),
            ..start_opts("/home/user/myproject")
        });
        assert!(
            cmd.contains("--model 'weird model'"),
            "expected shell-quoted model, got: {cmd}"
        );
    }

    #[test]
    fn resume_command_no_session_id() {
        let cmd = backend().build_resume_command(&resume_opts("/home/user/myproject", None));
        assert_eq!(
            cmd,
            Some("cd '/home/user/myproject' && claude --continue".to_string())
        );
    }

    #[test]
    fn resume_command_with_session_id() {
        let cmd =
            backend().build_resume_command(&resume_opts("/home/user/myproject", Some("abc123")));
        assert_eq!(
            cmd,
            Some("cd '/home/user/myproject' && claude --resume 'abc123'".to_string())
        );
    }

    #[test]
    fn resume_command_with_session_id_and_named_worktree() {
        let cmd = backend().build_resume_command(&ResumeOpts {
            worktree: Some(WorktreeMode::Named("feature-x".to_string())),
            ..resume_opts("/home/user/myproject", Some("abc123"))
        });
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && claude --resume 'abc123' --worktree 'feature-x'"
                    .to_string()
            )
        );
    }

    #[test]
    fn resume_command_with_model_and_effort() {
        let cmd = backend().build_resume_command(&ResumeOpts {
            model: Some("sonnet".into()),
            effort: Some("max".into()),
            ..resume_opts("/home/user/myproject", Some("abc123"))
        });
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && claude --resume 'abc123' --model 'sonnet' --effort 'max'"
                    .to_string()
            )
        );
    }

    #[test]
    fn start_command_with_permission_mode() {
        let cmd = backend().build_start_command(&StartOpts {
            permission_mode: Some("bypassPermissions".into()),
            ..start_opts("/home/user/myproject")
        });
        assert_eq!(
            cmd,
            "cd '/home/user/myproject' && claude --permission-mode 'bypassPermissions'"
        );
    }

    #[test]
    fn resume_command_with_permission_mode() {
        let cmd = backend().build_resume_command(&ResumeOpts {
            permission_mode: Some("bypassPermissions".into()),
            ..resume_opts("/home/user/myproject", Some("abc123"))
        });
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && claude --permission-mode 'bypassPermissions' --resume 'abc123'"
                    .to_string()
            )
        );
    }

    #[test]
    fn format_model_effort_flags_empty_when_none() {
        assert_eq!(format_model_effort_flags(None, None), "");
    }

    #[test]
    fn format_model_effort_flags_model_only() {
        assert_eq!(
            format_model_effort_flags(Some("sonnet"), None),
            " --model 'sonnet'"
        );
    }

    #[test]
    fn format_model_effort_flags_effort_only() {
        assert_eq!(
            format_model_effort_flags(None, Some("max")),
            " --effort 'max'"
        );
    }

    #[test]
    fn format_model_effort_flags_both() {
        assert_eq!(
            format_model_effort_flags(Some("opus"), Some("high")),
            " --model 'opus' --effort 'high'"
        );
    }

    #[test]
    fn format_model_effort_flags_drops_empty_strings() {
        // Defensive guard against empty/whitespace values that slipped past
        // the API boundary: omit the flag rather than emitting claude --model ''.
        assert_eq!(format_model_effort_flags(Some(""), Some("   ")), "");
        assert_eq!(
            format_model_effort_flags(Some("   "), Some("max")),
            " --effort 'max'"
        );
        assert_eq!(
            format_model_effort_flags(Some("sonnet"), Some("")),
            " --model 'sonnet'"
        );
    }

    #[test]
    fn detect_session_id_nonexistent_dir() {
        let result = backend().detect_session_id("/nonexistent/path/that/does/not/exist");
        assert_eq!(result, None);
    }

    #[test]
    fn resolve_project_root_strips_worktree_suffix() {
        let b = backend();
        assert_eq!(
            b.resolve_project_root("/home/user/myproject/.claude/worktrees/feature-x"),
            "/home/user/myproject"
        );
    }

    #[test]
    fn resolve_project_root_normal_path_unchanged() {
        let b = backend();
        assert_eq!(
            b.resolve_project_root("/home/user/myproject"),
            "/home/user/myproject"
        );
    }

    #[test]
    fn version_mismatch_none_when_previous_missing() {
        assert_eq!(version_mismatch_to_report(None, "1.2.3"), None);
    }

    #[test]
    fn version_mismatch_none_when_previous_empty() {
        assert_eq!(version_mismatch_to_report(Some(""), "1.2.3"), None);
        assert_eq!(version_mismatch_to_report(Some("   \n"), "1.2.3"), None);
    }

    #[test]
    fn version_mismatch_none_when_match() {
        assert_eq!(version_mismatch_to_report(Some("1.2.3"), "1.2.3"), None);
        // Trailing newline (how `std::fs::write` of the version would behave
        // if we ever started appending one) should not count as a mismatch.
        assert_eq!(version_mismatch_to_report(Some("1.2.3\n"), "1.2.3"), None);
    }

    #[test]
    fn version_mismatch_reports_trimmed_previous() {
        assert_eq!(
            version_mismatch_to_report(Some("1.2.2\n"), "1.2.3"),
            Some("1.2.2".to_string())
        );
    }
}
