use std::path::Path;

use super::{CodingAssistant, DeliveryMode, InjectConfig, ResumeOpts, StartOpts};

#[derive(Debug)]
pub struct ClaudeCode;

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
        match &opts.worktree {
            None => {
                format!("cd {escaped_dir} && claude --dangerously-skip-permissions")
            }
            Some(super::WorktreeMode::Disposable) => {
                format!("cd {escaped_dir} && claude --dangerously-skip-permissions --worktree")
            }
            Some(super::WorktreeMode::Named(name)) => {
                let escaped_name = crate::scheduler::shell_escape(name);
                format!(
                    "cd {escaped_dir} && claude --dangerously-skip-permissions --worktree {escaped_name}"
                )
            }
        }
    }

    fn build_resume_command(&self, opts: &ResumeOpts) -> Option<String> {
        let escaped_dir = crate::scheduler::shell_escape(&opts.project_dir);
        let resume_flag = match &opts.session_id {
            Some(sid) => format!("--resume {}", crate::scheduler::shell_escape(sid)),
            None => "--continue".to_string(),
        };
        let cmd = match &opts.worktree {
            None => {
                format!("cd {escaped_dir} && claude --dangerously-skip-permissions {resume_flag}")
            }
            Some(super::WorktreeMode::Disposable) => {
                format!(
                    "cd {escaped_dir} && claude --dangerously-skip-permissions {resume_flag} --worktree"
                )
            }
            Some(super::WorktreeMode::Named(name)) => {
                let escaped_name = crate::scheduler::shell_escape(name);
                format!(
                    "cd {escaped_dir} && claude --dangerously-skip-permissions {resume_flag} --worktree {escaped_name}"
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

    fn exit_command(&self) -> Option<&str> {
        Some("/exit")
    }

    fn install(&self) -> anyhow::Result<()> {
        todo!("will be moved from main.rs in Task 9")
    }

    fn is_available(&self) -> bool {
        std::process::Command::new("claude")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn description_file_priority(&self) -> &[&str] {
        &["CLAUDE.md", "README.md"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ResumeOpts, StartOpts, WorktreeMode};

    fn backend() -> ClaudeCode {
        ClaudeCode
    }

    #[test]
    fn start_command_no_worktree() {
        let cmd = backend().build_start_command(&StartOpts {
            project_dir: "/home/user/myproject".to_string(),
            worktree: None,
        });
        assert_eq!(
            cmd,
            "cd '/home/user/myproject' && claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn start_command_named_worktree() {
        let cmd = backend().build_start_command(&StartOpts {
            project_dir: "/home/user/myproject".to_string(),
            worktree: Some(WorktreeMode::Named("feature-x".to_string())),
        });
        assert_eq!(
            cmd,
            "cd '/home/user/myproject' && claude --dangerously-skip-permissions --worktree 'feature-x'"
        );
    }

    #[test]
    fn start_command_disposable_worktree() {
        let cmd = backend().build_start_command(&StartOpts {
            project_dir: "/home/user/myproject".to_string(),
            worktree: Some(WorktreeMode::Disposable),
        });
        assert_eq!(
            cmd,
            "cd '/home/user/myproject' && claude --dangerously-skip-permissions --worktree"
        );
    }

    #[test]
    fn resume_command_no_session_id() {
        let cmd = backend().build_resume_command(&ResumeOpts {
            project_dir: "/home/user/myproject".to_string(),
            session_id: None,
            worktree: None,
        });
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && claude --dangerously-skip-permissions --continue"
                    .to_string()
            )
        );
    }

    #[test]
    fn resume_command_with_session_id() {
        let cmd = backend().build_resume_command(&ResumeOpts {
            project_dir: "/home/user/myproject".to_string(),
            session_id: Some("abc123".to_string()),
            worktree: None,
        });
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && claude --dangerously-skip-permissions --resume 'abc123'"
                    .to_string()
            )
        );
    }

    #[test]
    fn resume_command_with_session_id_and_named_worktree() {
        let cmd = backend().build_resume_command(&ResumeOpts {
            project_dir: "/home/user/myproject".to_string(),
            session_id: Some("abc123".to_string()),
            worktree: Some(WorktreeMode::Named("feature-x".to_string())),
        });
        assert_eq!(
            cmd,
            Some(
                "cd '/home/user/myproject' && claude --dangerously-skip-permissions --resume 'abc123' --worktree 'feature-x'"
                    .to_string()
            )
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
}
