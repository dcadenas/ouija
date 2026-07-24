use std::process::Command;

/// Set a user variable on a tmux pane (`tmux set -pt <pane> <name> <value>`).
pub fn set(pane: &str, name: &str, value: &str) -> anyhow::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    let output = Command::new("tmux")
        .args(["set", "-t", pane, "-p", name, value])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to set tmux pane variable {name} on {pane}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Read the `@ouija_session` user variable from a tmux pane.
pub fn get(pane: &str) -> Option<String> {
    if cfg!(test) {
        return None;
    }
    let output = Command::new("tmux")
        .args(["display", "-p", "-t", pane, "#{@ouija_session}"])
        .output()
        .ok()?;
    let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if val.is_empty() { None } else { Some(val) }
}

/// Clear a user variable from a tmux pane (`tmux set -pu -t <pane> <name>`).
pub fn clear(pane: &str, name: &str) {
    if cfg!(test) {
        return;
    }
    let _ = Command::new("tmux")
        .args(["set", "-t", pane, "-pu", name])
        .status();
}
