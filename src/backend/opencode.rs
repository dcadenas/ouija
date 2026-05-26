use std::path::Path;

use super::{CodingAssistant, DeliveryMode, InjectConfig, ResumeOpts, StartOpts};

mod embedded {
    pub const PLUGIN_TS: &str = include_str!("../../opencode-plugin/ouija.ts");
    pub const SKILL_MD: &str = include_str!("../../skills/ouija/SKILL.md");
}

/// The legacy MCP URL that older ouija installs wrote into opencode's
/// `mcp.ouija` config. The `/mcp` route was removed from the daemon in
/// commit 2878926 "drop MCP tools, skill-only HATEOAS interface", and
/// any session that still has this entry keeps seeing SSE 404s from
/// opencode. We recognize it so we can clean it up.
const STALE_MCP_URL_PREFIX: &str = "http://localhost:7880/mcp";

/// Remove the dead `mcp.ouija` entry from an opencode config.
///
/// Only prunes if the entry's `url` points at `localhost:7880/mcp` (the
/// daemon's removed endpoint). User-provided custom URLs are left alone.
/// If pruning empties the surrounding `mcp` block entirely, the block
/// is removed too so configs don't accumulate empty objects.
fn prune_stale_mcp_ouija(config: &mut serde_json::Value) {
    let Some(obj) = config.as_object_mut() else {
        return;
    };
    let Some(mcp) = obj.get_mut("mcp").and_then(|v| v.as_object_mut()) else {
        return;
    };
    let stale = mcp
        .get("ouija")
        .and_then(|v| v.get("url"))
        .and_then(|v| v.as_str())
        .is_some_and(|u| u.starts_with(STALE_MCP_URL_PREFIX));
    if stale {
        mcp.remove("ouija");
    }
    if mcp.is_empty() {
        obj.remove("mcp");
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
        Some(format!(
            "cd {escaped_dir} && echo 'waiting for opencode attach...'"
        ))
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

        // Write the ouija skill for OpenCode's skill discovery
        let skills_dir = config_dir.join("skills/ouija");
        std::fs::create_dir_all(&skills_dir)?;
        std::fs::write(skills_dir.join("SKILL.md"), embedded::SKILL_MD)?;

        let mut config: serde_json::Value = match std::fs::read_to_string(&config_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({})),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&config_dir)?;
                serde_json::json!({ "$schema": "https://opencode.ai/config.json" })
            }
            Err(e) => return Err(e.into()),
        };

        // The `/mcp` route was removed from the daemon in commit 2878926.
        // Older installs wrote `mcp.ouija → http://localhost:7880/mcp`
        // into opencode.json, which causes persistent SSE 404 errors in
        // opencode's MCP sidebar. Prune the stale entry if present and
        // do NOT write a new one — ouija is skill+REST only now.
        prune_stale_mcp_ouija(&mut config);

        let obj = config
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("opencode config is not a JSON object"))?;

        // Add plugin to the plugin array (merge, don't overwrite)
        let plugin_file = plugins_dir.join("ouija.ts");
        let plugin_path = format!("file://{}", plugin_file.display());
        let plugins = obj.entry("plugin").or_insert_with(|| serde_json::json!([]));
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
            model: None,
            effort: None,
            permission_mode: None,
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
            model: None,
            effort: None,
            permission_mode: None,
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
    fn plugin_prompt_uses_public_session_id_for_sender_examples() {
        assert!(
            embedded::PLUGIN_TS.contains("ouija ask TARGET \"question\" --from ${publicSessionId}"),
            "OpenCode prompt must teach non-tmux tools to send from the resolved public Ouija session id"
        );
        assert!(
            embedded::PLUGIN_TS.contains("ouija tell TARGET \"info\" --from ${publicSessionId}"),
            "OpenCode prompt must not imply the backend label is a valid sender id"
        );
        assert!(
            embedded::PLUGIN_TS.contains("ouija reply TARGET N \"result\" --from ${publicSessionId}"),
            "OpenCode prompt must use the public session id for replies"
        );
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
    fn prune_stale_mcp_ouija_removes_legacy_url() {
        // Commit 2878926 dropped the /mcp route from the daemon, but the
        // install logic kept writing mcp.ouija. Anyone who ran an older
        // ouija still has this dead entry in their opencode.json.
        let mut config = serde_json::json!({
            "mcp": {
                "ouija": {
                    "type": "remote",
                    "url": "http://localhost:7880/mcp",
                    "oauth": false,
                },
                "other": { "type": "local", "command": ["echo"] },
            }
        });
        prune_stale_mcp_ouija(&mut config);
        assert!(
            config["mcp"].get("ouija").is_none(),
            "stale mcp.ouija should be removed, got {config:#}"
        );
        assert!(
            config["mcp"].get("other").is_some(),
            "unrelated mcp entries must be preserved, got {config:#}"
        );
    }

    #[test]
    fn prune_stale_mcp_ouija_leaves_non_default_url_alone() {
        // If a user has manually pointed mcp.ouija at some other URL
        // (e.g. a hand-rolled MCP bridge), don't clobber their config.
        let mut config = serde_json::json!({
            "mcp": {
                "ouija": {
                    "type": "remote",
                    "url": "https://example.internal/ouija-mcp",
                    "oauth": false,
                }
            }
        });
        prune_stale_mcp_ouija(&mut config);
        assert!(
            config["mcp"]["ouija"].is_object(),
            "custom mcp.ouija URL must be preserved, got {config:#}"
        );
    }

    #[test]
    fn prune_stale_mcp_ouija_tolerates_missing_mcp_block() {
        let mut config = serde_json::json!({ "plugin": [] });
        // Must not panic, must not inject an `mcp` block.
        prune_stale_mcp_ouija(&mut config);
        assert!(
            config.get("mcp").is_none(),
            "should not add an mcp block when none exists, got {config:#}"
        );
    }

    #[test]
    fn prune_stale_mcp_ouija_removes_empty_mcp_after_pruning() {
        // If mcp.ouija was the only entry, the whole mcp block becomes
        // empty — clean it up so the user's config doesn't accrue noise.
        let mut config = serde_json::json!({
            "mcp": {
                "ouija": {
                    "type": "remote",
                    "url": "http://localhost:7880/mcp",
                }
            }
        });
        prune_stale_mcp_ouija(&mut config);
        assert!(
            config.get("mcp").is_none() || config["mcp"].as_object().is_some_and(|m| m.is_empty()),
            "mcp block should be removed or empty after prune, got {config:#}"
        );
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
