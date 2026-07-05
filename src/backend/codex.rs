use std::path::Path;

use super::{CodingAssistant, DeliveryMode, InjectConfig, ResumeOpts, StartOpts};

/// Autonomy flags for Ouija-launched Codex sessions.
///
/// `--ask-for-approval never` + `--sandbox workspace-write` gives a bounded,
/// non-interactive session (writes confined to the workspace, no per-command
/// approval prompts that would stall tmux injection). `--no-alt-screen` keeps
/// terminal scrollback so pane capture and debugging work. All three are
/// verified present on both `codex` and `codex resume` (#1442). Fully
/// unrestricted runs (`--dangerously-bypass-approvals-and-sandbox`) are left to
/// externally-sandboxed setups and are not emitted here.
const AUTONOMY_FLAGS: &str = "--ask-for-approval never --sandbox workspace-write --no-alt-screen";

/// Render a ` --model <X>` fragment for the codex CLI, or an empty string.
///
/// The value is shell-escaped so it embeds safely in the surrounding
/// `format!`-built command. Empty / whitespace-only values are treated as
/// absent — emitting `codex --model ''` would just fail at the CLI, so omitting
/// the flag is the safer default. Codex has no verified `--effort` flag, so
/// reasoning effort is intentionally not mapped here.
fn format_model_flag(model: Option<&str>) -> String {
    match model.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => format!(" --model {}", crate::scheduler::shell_escape(m)),
        None => String::new(),
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
        // WorktreeMode is intentionally ignored: Codex CLI has no verified
        // `--worktree` flag. Ouija sets up the worktree/cwd before launch and
        // Codex is started inside it. `effort` is intentionally ignored too:
        // Codex CLI exposes no verified `--effort` flag (#1442).
        format!("cd {escaped_dir} && codex {AUTONOMY_FLAGS}{model}")
    }

    fn build_resume_command(&self, opts: &ResumeOpts) -> Option<String> {
        let escaped_dir = crate::scheduler::shell_escape(&opts.project_dir);
        let model = format_model_flag(opts.model.as_deref());
        // `codex resume --last` is the documented non-picker path for
        // continuing the most recent session in this cwd; an explicit
        // SESSION_ID targets a specific thread. WorktreeMode/effort ignored as
        // in `build_start_command`.
        let target = match &opts.session_id {
            Some(sid) => crate::scheduler::shell_escape(sid),
            None => "--last".to_string(),
        };
        Some(format!(
            "cd {escaped_dir} && codex resume {target} {AUTONOMY_FLAGS}{model}"
        ))
    }

    fn detect_session_id(&self, _project_dir: &str) -> Option<String> {
        // Codex records sessions globally under `$CODEX_HOME/sessions`, not in
        // a per-project directory. Ouija resumes via `codex resume --last`
        // rather than threading an opaque backend session id, so there is
        // nothing to auto-detect for v1.
        None
    }

    fn tui_ready_pattern(&self) -> Option<&str> {
        // Codex's interactive prompt glyph is U+203A (SINGLE RIGHT-POINTING
        // ANGLE QUOTATION MARK), observed as the visible `›` prompt.
        Some("\u{203A}")
    }

    fn inject_config(&self) -> InjectConfig {
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
        // Hook bootstrapping is added in a later chunk (Codex hooks.json +
        // scripts under ~/.codex). No-op for now so registration compiles.
        Ok(())
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
            "cd '/home/user/myproject' && codex --ask-for-approval never --sandbox workspace-write --no-alt-screen"
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
            "cd '/home/user/myproject' && codex --ask-for-approval never --sandbox workspace-write --no-alt-screen --model 'gpt-5.5'"
        );
    }

    #[test]
    fn start_command_ignores_worktree_and_effort() {
        // Codex has no verified --worktree or --effort flag; both must be
        // dropped rather than guessed onto the command line (#1442).
        let cmd = backend().build_start_command(&StartOpts {
            worktree: Some(WorktreeMode::Named("feature-x".to_string())),
            effort: Some("high".into()),
            ..start_opts("/home/user/myproject")
        });
        assert!(!cmd.contains("--worktree"), "must not emit --worktree: {cmd}");
        assert!(!cmd.contains("--effort"), "must not emit --effort: {cmd}");
        assert!(!cmd.contains("feature-x"), "must not emit worktree name: {cmd}");
        assert!(!cmd.contains("high"), "must not emit effort value: {cmd}");
    }

    #[test]
    fn resume_command_without_session_id_uses_last() {
        let cmd = backend().build_resume_command(&resume_opts("/home/user/myproject", None));
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && codex resume --last --ask-for-approval never --sandbox workspace-write --no-alt-screen"
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
                "cd '/home/user/myproject' && codex resume 'abc-123' --ask-for-approval never --sandbox workspace-write --no-alt-screen"
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
                "cd '/home/user/myproject' && codex resume --last --ask-for-approval never --sandbox workspace-write --no-alt-screen --model 'gpt-5.5'"
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
        assert!(!cmd.contains("--worktree"), "must not emit --worktree: {cmd}");
    }

    #[test]
    fn format_model_flag_drops_empty() {
        assert_eq!(format_model_flag(None), "");
        assert_eq!(format_model_flag(Some("")), "");
        assert_eq!(format_model_flag(Some("   ")), "");
        assert_eq!(format_model_flag(Some("gpt-5.5")), " --model 'gpt-5.5'");
    }

    #[test]
    fn detect_session_id_always_none() {
        assert_eq!(backend().detect_session_id("/home/user/myproject"), None);
    }

    #[test]
    fn tui_ready_pattern_is_prompt_glyph() {
        assert_eq!(backend().tui_ready_pattern(), Some("\u{203A}"));
    }

    #[test]
    fn has_project_history_always_false() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".codex")).unwrap();
        // Even with a local .codex dir, history is global — no per-project marker.
        assert!(!backend().has_project_history(tmp.path()));
    }
}
