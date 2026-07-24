pub mod claude_code;
pub mod codex;
pub mod opencode;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// Identifies one session inside a coding-assistant backend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct BackendSessionIdentity {
    pub backend: String,
    pub session_id: String,
}

/// Default hard timeout for a backend availability probe (`cli --version`).
const AVAILABILITY_TIMEOUT: Duration = Duration::from_secs(3);
const MISE_TRUST_TIMEOUT: Duration = Duration::from_secs(3);
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
const MISE_CONFIGS: &[&str] = &[
    "mise.toml",
    ".mise.toml",
    "mise/config.toml",
    ".tool-versions",
];

/// Run `command` to completion, killing it if it outlives `timeout`.
///
/// Returns `Some(status)` if the process exited on its own within the deadline,
/// or `None` if it could not be spawned or was killed for exceeding `timeout`.
///
/// Some backend CLIs are npx/npm wrappers whose `--version` can hang while a
/// wrapper resolves packages online. A blocking `Command::output()` would then
/// stall daemon startup and every session-start registration (which probes
/// availability per backend). Bounding the wait keeps those paths responsive.
fn run_with_timeout(command: &mut Command, timeout: Duration) -> Option<std::process::ExitStatus> {
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Whether `cli_name --version` exits successfully within `AVAILABILITY_TIMEOUT`.
///
/// Shared by every backend's default `is_available`. Output is discarded; only
/// the exit status within the timeout matters.
fn cli_reports_version(cli_name: &str) -> bool {
    let mut command = Command::new(cli_name);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    matches!(run_with_timeout(&mut command, AVAILABILITY_TIMEOUT), Some(status) if status.success())
}

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
    for path in mise_config_paths(Path::new(dir)) {
        let attempt = run_mise_trust(Path::new("mise"), &path, MISE_TRUST_TIMEOUT);
        if attempt.success() {
            tracing::debug!(config = %path.display(), "mise config pre-trusted");
        } else {
            log_mise_trust_failure(&path, &attempt);
        }
    }
}

fn mise_config_paths(dir: &Path) -> Vec<PathBuf> {
    MISE_CONFIGS
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| path.exists())
        .collect()
}

#[derive(Debug)]
enum MiseTrustAttempt {
    Completed(Output),
    SpawnFailed(String),
    WaitFailed(String),
    TimedOut(Option<Output>),
}

impl MiseTrustAttempt {
    fn success(&self) -> bool {
        matches!(self, Self::Completed(output) if output.status.success())
    }

    fn status_code(&self) -> Option<i32> {
        match self {
            Self::Completed(output) | Self::TimedOut(Some(output)) => output.status.code(),
            Self::SpawnFailed(_) | Self::WaitFailed(_) | Self::TimedOut(None) => None,
        }
    }

    fn stdout(&self) -> String {
        match self {
            Self::Completed(output) | Self::TimedOut(Some(output)) => {
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
            Self::SpawnFailed(_) | Self::WaitFailed(_) | Self::TimedOut(None) => String::new(),
        }
    }

    fn stderr(&self) -> String {
        match self {
            Self::Completed(output) | Self::TimedOut(Some(output)) => {
                String::from_utf8_lossy(&output.stderr).into_owned()
            }
            Self::SpawnFailed(_) | Self::WaitFailed(_) | Self::TimedOut(None) => String::new(),
        }
    }
}

fn run_mise_trust(mise_bin: &Path, config_path: &Path, timeout: Duration) -> MiseTrustAttempt {
    let mut command = Command::new(mise_bin);
    command
        .arg("trust")
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_mise_trust_command(&mut command, timeout)
}

fn run_mise_trust_command(command: &mut Command, timeout: Duration) -> MiseTrustAttempt {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return MiseTrustAttempt::SpawnFailed(error.to_string()),
    };
    let stdout = child.stdout.take().map(read_child_pipe);
    let stderr = child.stderr.take().map(read_child_pipe);
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child
                    .wait()
                    .map(|status| {
                        MiseTrustAttempt::Completed(collect_output(status, stdout, stderr))
                    })
                    .unwrap_or_else(|error| MiseTrustAttempt::WaitFailed(error.to_string()));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return MiseTrustAttempt::TimedOut(
                        child
                            .wait()
                            .ok()
                            .map(|status| collect_output(status, stdout, stderr)),
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return MiseTrustAttempt::WaitFailed(error.to_string());
            }
        }
    }
}

fn read_child_pipe<T>(mut pipe: T) -> Receiver<Vec<u8>>
where
    T: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

fn collect_output(
    status: std::process::ExitStatus,
    stdout: Option<Receiver<Vec<u8>>>,
    stderr: Option<Receiver<Vec<u8>>>,
) -> Output {
    Output {
        status,
        stdout: join_pipe(stdout),
        stderr: join_pipe(stderr),
    }
}

fn join_pipe(receiver: Option<Receiver<Vec<u8>>>) -> Vec<u8> {
    let Some(receiver) = receiver else {
        return Vec::new();
    };
    let mut output = Vec::new();
    while let Ok(chunk) = receiver.recv_timeout(PIPE_DRAIN_TIMEOUT) {
        output.extend(chunk);
    }
    output
}

fn log_mise_trust_failure(path: &Path, attempt: &MiseTrustAttempt) {
    let stdout = truncate_for_log(&attempt.stdout());
    let stderr = truncate_for_log(&attempt.stderr());
    match attempt {
        MiseTrustAttempt::Completed(output) => {
            tracing::warn!(
                config = %path.display(),
                status = ?output.status,
                stdout = %stdout,
                stderr = %stderr,
                "mise trust exited unsuccessfully; spawned shells may prompt for trust"
            );
        }
        MiseTrustAttempt::SpawnFailed(error) => {
            tracing::warn!(
                config = %path.display(),
                error = %error,
                "mise trust could not be started; spawned shells may prompt for trust"
            );
        }
        MiseTrustAttempt::WaitFailed(error) => {
            tracing::warn!(
                config = %path.display(),
                error = %error,
                stdout = %stdout,
                stderr = %stderr,
                "mise trust wait failed; spawned shells may prompt for trust"
            );
        }
        MiseTrustAttempt::TimedOut(_) => {
            tracing::warn!(
                config = %path.display(),
                timeout_ms = MISE_TRUST_TIMEOUT.as_millis(),
                status_code = ?attempt.status_code(),
                stdout = %stdout,
                stderr = %stderr,
                "mise trust timed out; spawned shells may prompt for trust"
            );
        }
    }
}

fn truncate_for_log(value: &str) -> String {
    const MAX_CHARS: usize = 1000;
    let mut chars = value.trim().chars();
    let truncated: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
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
                Arc::new(codex::Codex) as _,
            ],
            "claude-code",
        )
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn CodingAssistant>> {
        self.backends.iter().find(|b| b.name() == name).cloned()
    }

    pub fn names(&self) -> Vec<&str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    pub fn valid_names_csv(&self) -> String {
        self.names().join(", ")
    }

    pub fn unknown_backend_message(&self, name: &str) -> String {
        format!(
            "unknown backend '{name}'. Valid backends: {}",
            self.valid_names_csv()
        )
    }

    pub fn get_required(&self, name: &str) -> Result<Arc<dyn CodingAssistant>, String> {
        self.get(name)
            .ok_or_else(|| self.unknown_backend_message(name))
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

    /// Resolve the current tool shell's backend-native identity.
    ///
    /// Returns `None` when no backend exposes an identity or when more than
    /// one backend claims the shell. Ambiguous identity must fail closed.
    pub fn caller_session_identity(&self) -> Option<BackendSessionIdentity> {
        let mut identities = self.backends.iter().filter_map(|backend| {
            backend
                .caller_session_id()
                .map(|session_id| BackendSessionIdentity {
                    backend: backend.name().to_string(),
                    session_id,
                })
        });
        let identity = identities.next()?;
        identities.next().is_none().then_some(identity)
    }

    /// Every registered backend paired with its process names, regardless of
    /// availability.
    ///
    /// Process-tree detection (`detect_backend_in_pane`) matches a running pane's
    /// process names against this set. It must NOT be filtered by `available()`:
    /// that runs each backend's `is_available()` CLI probe (e.g. a slow npx
    /// `codex --version`), which both blocks the caller and would drop a live
    /// pane whenever its backend CLI is slow to answer.
    pub fn all_backend_process_names(&self) -> Vec<(String, Vec<String>)> {
        self.backends
            .iter()
            .map(|b| {
                (
                    b.name().to_string(),
                    b.process_names().iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect()
    }

    /// Whether `backend_name` delivers messages over HTTP rather than the tmux TUI.
    ///
    /// HTTP-delivered sessions (e.g. opencode on a shared serve) reach the
    /// backend through its API independently of the tmux pane, so pane-process
    /// liveness is not a death signal for them: the attach TUI can die — or
    /// never start, on version skew — while the session stays fully reachable.
    /// Returns `false` for unknown backend names.
    pub fn uses_http_delivery(&self, backend_name: &str) -> bool {
        self.get(backend_name)
            .is_some_and(|b| matches!(b.delivery_mode(), DeliveryMode::HttpApi { .. }))
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
    /// Claude Code permission mode override. Other backends ignore this.
    pub permission_mode: Option<String>,
    /// CODEX_HOME override. Only the Codex backend uses this.
    pub codex_home: Option<String>,
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
    /// Claude Code permission mode override. Other backends ignore this.
    pub permission_mode: Option<String>,
    /// CODEX_HOME override. Only the Codex backend uses this.
    pub codex_home: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchModelConfig {
    pub model: Option<String>,
    pub codex_home: Option<String>,
}

/// Resolve user-facing model aliases into backend-specific launch config.
///
/// The public session interface stays backend-agnostic (`--backend` +
/// `--model`). Codex-specific provider homes live in settings as model routes,
/// so callers can pass `--model gemini` without knowing that Codex needs a
/// separate `CODEX_HOME` for the Gemini sidecar setup.
pub(crate) fn resolve_launch_model_config(
    backend_name: &str,
    model: Option<String>,
    settings: &crate::persistence::OuijaSettings,
) -> LaunchModelConfig {
    if backend_name == "codex-cli" {
        if let Some(alias) = model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            if let Some(route) = settings.codex_model_routes.get(alias) {
                return LaunchModelConfig {
                    model: route.model.clone().or_else(|| Some(alias.to_string())),
                    codex_home: route
                        .codex_home
                        .clone()
                        .or_else(|| settings.codex_home.clone()),
                };
            }
        }
        return LaunchModelConfig {
            model,
            codex_home: settings.codex_home.clone(),
        };
    }

    LaunchModelConfig {
        model,
        codex_home: None,
    }
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
    /// Resolve this backend's session ID from the current tool environment.
    fn caller_session_id(&self) -> Option<String> {
        None
    }
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
        cli_reports_version(self.cli_name())
    }
    fn description_file_priority(&self) -> &[&str] {
        &["README.md"]
    }
}

#[cfg(test)]
pub(crate) fn assert_shared_task_reminder_guidance(skill: &str) {
    assert!(
        skill.contains("`--reminder` alone opts the session into recurring recovery nudges."),
        "skill must make recurring reminders explicitly opt in"
    );
    assert!(
        skill.contains("`--when-done keep-open|ask-parent|close`"),
        "skill must teach the primary completion option independently"
    );
    assert!(
        skill.contains("`--idle-policy` is deprecated"),
        "skill must label the compatibility option as deprecated"
    );
    assert!(
        skill.contains("Pending replies can still wake a session without `--reminder`."),
        "skill must preserve the independent pending-reply wakeup contract"
    );
    assert!(
        skill.contains("Never put `ouija clear-reminder` in manual reminder text."),
        "skill must reserve generated clearing commands for Ouija"
    );
    let placeholder_command = ["ouija clear-reminder", "N"].join(" ");
    assert!(
        !skill.contains(&placeholder_command),
        "skill must not contain a copyable placeholder clearing command"
    );
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

    #[test]
    fn codex_without_model_uses_default_home_resolution() {
        let settings = crate::persistence::OuijaSettings::default();
        let launch = resolve_launch_model_config("codex-cli", None, &settings);
        assert_eq!(
            launch,
            LaunchModelConfig {
                model: None,
                codex_home: None,
            }
        );
    }

    #[test]
    fn codex_model_alias_resolves_route() {
        let mut settings = crate::persistence::OuijaSettings::default();
        settings.codex_model_routes.insert(
            "gemini".into(),
            crate::persistence::CodexModelRoute {
                model: Some("gemini-2.5-pro".into()),
                codex_home: Some("~/.cache/codex-gemini".into()),
            },
        );

        let launch = resolve_launch_model_config("codex-cli", Some("gemini".into()), &settings);
        assert_eq!(
            launch,
            LaunchModelConfig {
                model: Some("gemini-2.5-pro".into()),
                codex_home: Some("~/.cache/codex-gemini".into()),
            }
        );
    }

    #[test]
    fn run_with_timeout_returns_status_for_fast_success() {
        let status = run_with_timeout(&mut Command::new("true"), Duration::from_secs(3));
        assert!(status.is_some_and(|s| s.success()));
    }

    #[test]
    fn run_with_timeout_returns_status_for_fast_failure() {
        let status = run_with_timeout(&mut Command::new("false"), Duration::from_secs(3));
        assert!(status.is_some_and(|s| !s.success()));
    }

    #[test]
    fn run_with_timeout_kills_and_returns_none_when_deadline_exceeded() {
        // `sleep 5` would never finish inside a 200ms budget. The helper must
        // give up promptly rather than block — this is the guarantee that keeps
        // a hanging `codex --version` wrapper from stalling daemon startup.
        let start = Instant::now();
        let status = run_with_timeout(Command::new("sleep").arg("5"), Duration::from_millis(200));
        assert!(status.is_none(), "timed-out process must return None");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "helper must return near the deadline, not wait for the process"
        );
    }

    #[test]
    fn run_with_timeout_returns_none_for_missing_binary() {
        let status = run_with_timeout(
            &mut Command::new("ouija-nonexistent-binary-xyz"),
            Duration::from_secs(3),
        );
        assert!(status.is_none());
    }

    #[test]
    fn cli_reports_version_true_for_command_that_exits_zero() {
        // `true` ignores `--version` and exits 0, so the probe reports available.
        assert!(cli_reports_version("true"));
    }

    #[test]
    fn cli_reports_version_false_for_missing_binary() {
        assert!(!cli_reports_version("ouija-nonexistent-binary-xyz"));
    }

    #[test]
    fn mise_config_paths_only_include_existing_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("mise.toml"), "[tools]\n").unwrap();
        std::fs::create_dir(root.join("mise")).unwrap();
        std::fs::write(root.join("mise/config.toml"), "[env]\n").unwrap();

        let paths = mise_config_paths(root);

        assert_eq!(
            paths,
            vec![root.join("mise.toml"), root.join("mise/config.toml")]
        );
    }

    #[test]
    fn mise_trust_attempt_captures_nonzero_output() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_mise = tmp.path().join("mise");
        std::fs::write(
            &fake_mise,
            "#!/bin/sh\nprintf 'out line\\n'\nprintf 'err line\\n' >&2\nexit 42\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_mise, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = tmp.path().join("mise.toml");
        std::fs::write(&config, "[tools]\n").unwrap();

        let attempt = run_mise_trust(&fake_mise, &config, Duration::from_secs(1));

        assert_eq!(attempt.status_code(), Some(42));
        assert_eq!(attempt.stdout(), "out line\n");
        assert_eq!(attempt.stderr(), "err line\n");
    }

    #[test]
    fn mise_trust_timeout_does_not_wait_for_pipe_holding_descendant() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_mise = tmp.path().join("mise");
        std::fs::write(
            &fake_mise,
            "#!/bin/sh\n(sleep 3) &\nprintf 'before timeout\\n'\nsleep 3\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_mise, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = tmp.path().join("mise.toml");
        std::fs::write(&config, "[tools]\n").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let fake_mise = fake_mise.clone();
        std::thread::spawn(move || {
            let attempt = run_mise_trust(&fake_mise, &config, Duration::from_millis(100));
            let _ = tx.send(attempt.stdout());
        });

        let stdout = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("timeout path must not wait for descendant-held stdout pipe");
        assert_eq!(stdout, "before timeout\n");
    }

    #[test]
    fn actual_mise_trust_suppresses_untrusted_config_check_when_available() {
        if !cli_reports_version("mise") {
            eprintln!("skipping actual mise trust test because mise is not on PATH");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let mise_data = tmp.path().join("mise-data");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&mise_data).unwrap();
        let config = project.join("mise.toml");
        std::fs::write(
            &config,
            "[env]\nOUIJA_MISE_TRUST_TEST = \"trusted-after-command\"\n",
        )
        .unwrap();

        let before = std::process::Command::new("mise")
            .arg("env")
            .arg("-C")
            .arg(&project)
            .env("MISE_DATA_DIR", &mise_data)
            .output()
            .unwrap();
        assert!(
            !before.status.success(),
            "untrusted mise config should fail before trust"
        );
        assert!(
            String::from_utf8_lossy(&before.stderr).contains("not trusted"),
            "expected untrusted-config error, got stderr: {}",
            String::from_utf8_lossy(&before.stderr)
        );

        let trust = std::process::Command::new("mise")
            .arg("trust")
            .arg(&config)
            .env("MISE_DATA_DIR", &mise_data)
            .output()
            .unwrap();
        assert!(
            trust.status.success(),
            "mise trust failed: stdout={} stderr={}",
            String::from_utf8_lossy(&trust.stdout),
            String::from_utf8_lossy(&trust.stderr)
        );

        let after = std::process::Command::new("mise")
            .arg("env")
            .arg("-C")
            .arg(&project)
            .env("MISE_DATA_DIR", &mise_data)
            .output()
            .unwrap();
        assert!(
            after.status.success(),
            "trusted mise config should load without prompt: stderr={}",
            String::from_utf8_lossy(&after.stderr)
        );
    }

    #[test]
    fn uses_http_delivery_distinguishes_backends() {
        let registry = BackendRegistry::default_registry();
        // opencode runs on a shared serve and is reached over HTTP.
        assert!(registry.uses_http_delivery("opencode"));
        // claude-code is driven through the tmux TUI.
        assert!(!registry.uses_http_delivery("claude-code"));
        // codex-cli is driven through the tmux TUI, not HTTP.
        assert!(!registry.uses_http_delivery("codex-cli"));
        // Unknown backends default to false.
        assert!(!registry.uses_http_delivery("nonexistent"));
    }

    #[test]
    fn registry_includes_codex_backend() {
        let registry = BackendRegistry::default_registry();
        let codex = registry
            .get("codex-cli")
            .expect("codex-cli backend must be registered");
        assert_eq!(codex.cli_name(), "codex");
        // Its process name participates in the global process-name sweep.
        assert!(registry.all_process_names().iter().any(|n| n == "codex"));
    }

    /// A backend whose CLI binary does not exist, so `is_available()` is false.
    /// Used to prove process-tree detection does not gate on availability.
    #[derive(Debug)]
    struct UnavailableBackend;
    impl CodingAssistant for UnavailableBackend {
        fn name(&self) -> &str {
            "ghost"
        }
        fn cli_name(&self) -> &str {
            "ouija-nonexistent-binary-xyz"
        }
        fn process_names(&self) -> &[&str] {
            &["ghostproc"]
        }
        fn delivery_mode(&self) -> DeliveryMode {
            DeliveryMode::TuiInjection
        }
        fn build_start_command(&self, _: &StartOpts) -> String {
            String::new()
        }
        fn build_resume_command(&self, _: &ResumeOpts) -> Option<String> {
            None
        }
        fn detect_session_id(&self, _: &str) -> Option<String> {
            None
        }
        fn tui_ready_pattern(&self) -> Option<&str> {
            None
        }
        fn inject_config(&self) -> InjectConfig {
            InjectConfig {
                paste_settle_ms: 0,
                use_inner_bracketed_paste: false,
                startup_inject_delay_secs: 0,
            }
        }
        fn config_dir_name(&self) -> &str {
            ".ghost"
        }
        fn has_project_history(&self, _: &Path) -> bool {
            false
        }
        fn exit_command(&self) -> Option<&str> {
            None
        }
        fn install(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn all_backend_process_names_ignores_availability() {
        let registry = BackendRegistry::new(vec![Arc::new(UnavailableBackend) as _], "ghost");
        // The CLI binary is absent, so availability-based listing excludes it.
        assert!(registry.available().is_empty());
        // But process-tree detection must still know its process names, so a
        // live pane running that backend is never dropped just because its CLI
        // is slow or absent when asked for --version (the codex npx-wrapper bug).
        let names = registry.all_backend_process_names();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].0, "ghost");
        assert_eq!(names[0].1, vec!["ghostproc".to_string()]);
    }

    #[test]
    fn all_backend_process_names_covers_every_default_backend() {
        let registry = BackendRegistry::default_registry();
        let names = registry.all_backend_process_names();
        for backend in ["claude-code", "opencode", "codex-cli"] {
            assert!(
                names.iter().any(|(n, _)| n == backend),
                "{backend} missing from detection candidate set: {names:?}"
            );
        }
    }
}
