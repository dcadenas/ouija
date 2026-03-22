use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;

use super::{CodingAssistant, DeliveryMode, InjectConfig, ResumeOpts, StartOpts};

mod embedded {
    pub const PLUGIN_TS: &str = include_str!("../../opencode-plugin/ouija.ts");
}

/// Find the opencode binary by checking common locations and PATH.
fn which_opencode() -> Option<PathBuf> {
    // Check common install locations first (avoids spawning `which`)
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(&home).join(".local/bin/opencode");
        if p.exists() {
            return Some(p);
        }
    }
    for loc in ["/usr/local/bin/opencode", "/usr/bin/opencode"] {
        let p = PathBuf::from(loc);
        if p.exists() {
            return Some(p);
        }
    }
    // Fall back to PATH lookup
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let p = PathBuf::from(dir).join("opencode");
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// Manages a shared `opencode serve` instance for the daemon.
///
/// One serve process runs per ouija daemon. Individual sessions create
/// opencode sessions via the HTTP API and run `opencode attach` in their
/// tmux panes.
pub struct OpenCodeServe {
    port: Mutex<Option<u16>>,
    process_pid: Mutex<Option<u32>>,
}

impl std::fmt::Debug for OpenCodeServe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenCodeServe")
            .field("port", &self.port.lock().unwrap())
            .field("process_pid", &self.process_pid.lock().unwrap())
            .finish()
    }
}

impl OpenCodeServe {
    pub fn new() -> Self {
        Self {
            port: Mutex::new(None),
            process_pid: Mutex::new(None),
        }
    }

    /// Ensure the serve process is running. Returns the port.
    /// Starts the process if not already running, or restarts if the
    /// health check fails.
    pub async fn ensure_running(&self, daemon_port: u16) -> anyhow::Result<u16> {
        // Check if already running and healthy (drop the guard before await)
        let existing_port = { *self.port.lock().unwrap() };
        if let Some(port) = existing_port {
            let health = reqwest::Client::new()
                .get(format!("http://127.0.0.1:{port}/global/health"))
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await;
            if health.is_ok() {
                return Ok(port);
            }
            // Serve died, clear state and restart
            tracing::warn!("opencode serve on port {port} failed health check, restarting");
            *self.port.lock().unwrap() = None;
            *self.process_pid.lock().unwrap() = None;
        }

        let serve_port = daemon_port + 320;

        let opencode_bin = which_opencode().unwrap_or_else(|| PathBuf::from("opencode"));
        tracing::info!("starting opencode serve on port {serve_port} (binary: {})", opencode_bin.display());
        let mut cmd = std::process::Command::new(&opencode_bin);
        cmd.args([
            "serve",
            "--port",
            &serve_port.to_string(),
            "--hostname",
            "127.0.0.1",
        ]);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        // Ensure PATH includes common opencode install locations
        if let Ok(path) = std::env::var("PATH") {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
            let extra = format!("{home}/.local/bin:/usr/local/bin:{path}");
            cmd.env("PATH", extra);
        }
        let child = cmd.spawn().context("failed to spawn opencode serve")?;

        let pid = child.id();
        {
            *self.process_pid.lock().unwrap() = Some(pid);
            *self.port.lock().unwrap() = Some(serve_port);
        }

        // Wait for readiness by polling the health endpoint
        let client = reqwest::Client::new();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if std::time::Instant::now() > deadline {
                // Try to read stderr for clues
                if let Some(pid) = *self.process_pid.lock().unwrap() {
                    let status = std::process::Command::new("kill")
                        .args(["-0", &pid.to_string()])
                        .status();
                    tracing::error!(
                        "opencode serve timed out on port {serve_port}, process alive: {:?}",
                        status.map(|s| s.success())
                    );
                }
                anyhow::bail!(
                    "opencode serve did not become ready within 30s on port {serve_port}"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let resp = client
                .get(format!("http://127.0.0.1:{serve_port}/global/global/health"))
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await;
            if resp.is_ok() {
                tracing::info!("opencode serve ready on port {serve_port} (pid {pid})");
                break;
            }
        }

        Ok(serve_port)
    }

    /// Create a new opencode session on the shared serve instance.
    pub async fn create_session(&self, client: &reqwest::Client) -> anyhow::Result<String> {
        let port = self
            .port
            .lock()
            .unwrap()
            .ok_or_else(|| anyhow::anyhow!("opencode serve not running"))?;
        let resp = client
            .post(format!("http://127.0.0.1:{port}/session"))
            .json(&serde_json::json!({}))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("opencode session creation failed {status}: {body}");
        }
        let body: serde_json::Value = resp.json().await?;
        body["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("no session id in opencode response"))
    }

    /// Get the current port, if serve is running.
    pub fn port(&self) -> Option<u16> {
        *self.port.lock().unwrap()
    }

    /// Stop the serve process.
    pub fn stop(&self) {
        if let Some(pid) = self.process_pid.lock().unwrap().take() {
            tracing::info!("stopping opencode serve (pid {pid})");
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
        *self.port.lock().unwrap() = None;
    }
}

impl Drop for OpenCodeServe {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug)]
pub struct OpenCode;

impl CodingAssistant for OpenCode {
    fn name(&self) -> &str {
        "opencode"
    }

    fn cli_name(&self) -> &str {
        "opencode"
    }

    fn process_names(&self) -> &[&str] {
        &["opencode"]
    }

    fn delivery_mode(&self) -> DeliveryMode {
        DeliveryMode::HttpApi {
            serve_command: "opencode serve".into(),
            attach_command: "opencode attach".into(),
            default_port: 0,
        }
    }

    fn build_start_command(&self, opts: &StartOpts) -> String {
        // Placeholder: HttpApi sessions use the shared serve, so this is
        // only called as a fallback. The actual attach command is built
        // by start_session/restart_session after creating the opencode session.
        let escaped_dir = crate::scheduler::shell_escape(&opts.project_dir);
        format!("cd {escaped_dir} && echo 'waiting for opencode attach...'")
    }

    fn build_resume_command(&self, opts: &ResumeOpts) -> Option<String> {
        // Resume is handled via HTTP API on the shared serve
        let escaped_dir = crate::scheduler::shell_escape(&opts.project_dir);
        Some(format!("cd {escaped_dir} && echo 'waiting for opencode attach...'"))
    }

    fn detect_session_id(&self, _project_dir: &str) -> Option<String> {
        None
    }

    fn tui_ready_pattern(&self) -> Option<&str> {
        None
    }

    fn inject_config(&self) -> InjectConfig {
        // Fallback values if tmux injection is ever used; HttpApi mode bypasses this.
        InjectConfig {
            paste_settle_ms: 100,
            use_inner_bracketed_paste: false,
            startup_inject_delay_secs: 0,
        }
    }

    fn config_dir_name(&self) -> &str {
        ".opencode"
    }

    fn resolve_project_root<'a>(&self, path: &'a str) -> &'a str {
        path
    }

    fn has_project_history(&self, dir: &Path) -> bool {
        dir.join(".opencode").is_dir()
    }

    fn exit_command(&self) -> Option<&str> {
        None
    }

    fn install(&self) -> anyhow::Result<()> {
        let home = std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .map_err(|_| anyhow::anyhow!("HOME environment variable not set"))?;

        let config_dir = home.join(".config/opencode");
        let config_path = config_dir.join("opencode.json");

        // Write the plugin file
        let plugins_dir = config_dir.join("plugins");
        std::fs::create_dir_all(&plugins_dir)?;
        std::fs::write(plugins_dir.join("ouija.ts"), embedded::PLUGIN_TS)?;

        let mut config: serde_json::Value = match std::fs::read_to_string(&config_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({})),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&config_dir)?;
                serde_json::json!({ "$schema": "https://opencode.ai/config.json" })
            }
            Err(e) => return Err(e.into()),
        };

        let obj = config
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("opencode config is not a JSON object"))?;

        // Add MCP config
        let mcp = obj
            .entry("mcp")
            .or_insert_with(|| serde_json::json!({}));

        if mcp.get("ouija").is_none() {
            mcp["ouija"] = serde_json::json!({
                "type": "remote",
                "url": "http://localhost:7880/mcp"
            });
        }

        // Add plugin to the plugin array (merge, don't overwrite)
        let plugin_file = plugins_dir.join("ouija.ts");
        let plugin_path = format!("file://{}", plugin_file.display());
        let plugins = obj
            .entry("plugin")
            .or_insert_with(|| serde_json::json!([]));
        if let Some(arr) = plugins.as_array_mut() {
            // Remove old relative-path entry if present
            arr.retain(|v| v.as_str() != Some("./plugins/ouija.ts"));
            if !arr.iter().any(|v| v.as_str() == Some(&plugin_path)) {
                arr.push(serde_json::json!(plugin_path));
            }
        }

        std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ResumeOpts, StartOpts};

    fn backend() -> OpenCode {
        OpenCode
    }

    #[test]
    fn start_command_basic() {
        let cmd = backend().build_start_command(&StartOpts {
            project_dir: "/home/user/myproject".to_string(),
            worktree: None,
        });
        // HttpApi backends use shared serve; start command is a placeholder
        assert!(cmd.contains("/home/user/myproject"));
    }

    #[test]
    fn resume_command_returns_some() {
        let cmd = backend().build_resume_command(&ResumeOpts {
            project_dir: "/home/user/myproject".to_string(),
            session_id: None,
            worktree: None,
        });
        assert!(cmd.is_some());
        assert!(cmd.unwrap().contains("/home/user/myproject"));
    }

    #[test]
    fn detect_session_id_always_none() {
        assert_eq!(backend().detect_session_id("/home/user/myproject"), None);
        assert_eq!(backend().detect_session_id("/some/other/path"), None);
    }

    #[test]
    fn has_project_history_with_opencode_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".opencode")).unwrap();
        assert!(backend().has_project_history(tmp.path()));
    }

    #[test]
    fn has_project_history_without_opencode_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!backend().has_project_history(tmp.path()));
    }

    #[test]
    fn resolve_project_root_unchanged() {
        let b = backend();
        assert_eq!(
            b.resolve_project_root("/home/user/myproject"),
            "/home/user/myproject"
        );
        assert_eq!(
            b.resolve_project_root("/home/user/myproject/subdir"),
            "/home/user/myproject/subdir"
        );
    }
}
