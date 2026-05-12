pub mod claude_code;
pub mod opencode;

use std::path::Path;
use std::sync::Arc;

/// Pre-trust mise config files in `dir` so spawned shells don't block on an
/// interactive "Trust them? [Yes/No/All]" prompt.
///
/// When a shell with `mise activate` sees an untrusted mise config, it prompts
/// for trust before loading shims. In HttpApi-backed sessions that prompt
/// blocks `opencode attach` forever — no `.opencode` descendant ever appears
/// in the pane tree, and the reaper's `pane_alive` check reaps the session
/// at the 60s grace boundary. Trusting the config non-interactively at spawn
/// time eliminates the stall.
///
/// Best-effort: no-op when mise isn't installed, when the dir has no mise
/// config, or when `mise trust` fails for any reason.
pub fn pre_trust_mise(dir: &str) {
    if cfg!(test) {
        return;
    }
    const CONFIGS: &[&str] = &[
        "mise.toml",
        ".mise.toml",
        "mise/config.toml",
        ".tool-versions",
    ];
    for name in CONFIGS {
        let path = format!("{dir}/{name}");
        if !std::path::Path::new(&path).exists() {
            continue;
        }
        let _ = std::process::Command::new("mise")
            .args(["trust", &path])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Registry of available coding assistant backends.
///
/// Holds all known backends and provides lookup by name plus a configurable
/// default. Global operations (e.g. scanning for any assistant process) use
/// `all_process_names()`, while per-session operations resolve the backend
/// via `get(name)`.
#[derive(Debug)]
pub struct BackendRegistry {
    backends: Vec<Arc<dyn CodingAssistant>>,
    default_name: String,
}

impl BackendRegistry {
    pub fn new(backends: Vec<Arc<dyn CodingAssistant>>, default: &str) -> Self {
        Self {
            backends,
            default_name: default.to_string(),
        }
    }

    pub fn default_registry() -> Self {
        Self::new(
            vec![
                Arc::new(claude_code::ClaudeCode) as _,
                Arc::new(opencode::OpenCode) as _,
            ],
            "claude-code",
        )
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn CodingAssistant>> {
        self.backends.iter().find(|b| b.name() == name).cloned()
    }

    pub fn default(&self) -> Arc<dyn CodingAssistant> {
        self.get(&self.default_name)
            .expect("default backend must exist")
    }

    /// Returns names of backends whose binary is found in PATH.
    pub fn available(&self) -> Vec<&str> {
        self.backends
            .iter()
            .filter(|b| b.is_available())
            .map(|b| b.name())
            .collect()
    }

    pub fn all_process_names(&self) -> Vec<String> {
        self.backends
            .iter()
            .flat_map(|b| b.process_names().iter().map(|s| s.to_string()))
            .collect()
    }
}

/// How a backend receives messages from ouija.
#[derive(Debug, Clone)]
pub enum DeliveryMode {
    /// Messages delivered via tmux paste-buffer injection into a TUI process.
    TuiInjection,
    /// Messages delivered via HTTP API to a headless server process.
    HttpApi {
        #[allow(dead_code)]
        serve_command: String,
        #[allow(dead_code)]
        attach_command: String,
    },
}

#[derive(Debug)]
pub struct StartOpts {
    pub project_dir: String,
    pub worktree: Option<WorktreeMode>,
    /// LLM model override (passed through to backend CLI / API).
    pub model: Option<String>,
    /// Reasoning effort / variant (passed through to backend CLI / API).
    pub effort: Option<String>,
}

#[derive(Debug)]
pub struct ResumeOpts {
    pub project_dir: String,
    pub session_id: Option<String>,
    pub worktree: Option<WorktreeMode>,
    /// LLM model override (passed through to backend CLI / API).
    pub model: Option<String>,
    /// Reasoning effort / variant (passed through to backend CLI / API).
    pub effort: Option<String>,
}

#[derive(Debug, Clone)]
pub enum WorktreeMode {
    Named(String),
    Disposable,
}

#[derive(Debug, Clone, Copy)]
pub struct InjectConfig {
    pub paste_settle_ms: u64,
    pub use_inner_bracketed_paste: bool,
    pub startup_inject_delay_secs: u64,
}

/// A terminal-based coding assistant that ouija can orchestrate.
#[allow(dead_code)]
pub trait CodingAssistant: Send + Sync + std::fmt::Debug + 'static {
    fn name(&self) -> &str;
    fn cli_name(&self) -> &str;
    fn process_names(&self) -> &[&str];
    fn delivery_mode(&self) -> DeliveryMode;
    fn build_start_command(&self, opts: &StartOpts) -> String;
    fn build_resume_command(&self, opts: &ResumeOpts) -> Option<String>;
    fn detect_session_id(&self, project_dir: &str) -> Option<String>;
    fn tui_ready_pattern(&self) -> Option<&str>;
    fn inject_config(&self) -> InjectConfig;
    fn config_dir_name(&self) -> &str;
    fn resolve_project_root<'a>(&self, path: &'a str) -> &'a str {
        path
    }
    fn has_project_history(&self, dir: &Path) -> bool;
    fn compact_command(&self) -> Option<&str> {
        None
    }
    fn exit_command(&self) -> Option<&str>;
    fn install(&self) -> anyhow::Result<()>;
    fn is_available(&self) -> bool {
        std::process::Command::new(self.cli_name())
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    fn description_file_priority(&self) -> &[&str] {
        &["README.md"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_available_returns_backends_with_binaries() {
        let registry = BackendRegistry::default_registry();
        let available = registry.available();
        assert!(available.iter().all(|name| !name.is_empty()));
    }
}
