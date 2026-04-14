use std::process::Command;

/// Set a user variable on a tmux pane (`tmux set -pt <pane> <name> <value>`).
pub fn set(pane: &str, name: &str, value: &str) {
    let _ = Command::new("tmux")
        .args(["set", "-t", pane, "-p", name, value])
        .status();
}

/// Read the `@ouija_session` user variable from a tmux pane.
pub fn get(pane: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["display", "-p", "-t", pane, "#{@ouija_session}"])
        .output()
        .ok()?;
    let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if val.is_empty() { None } else { Some(val) }
}

/// Clear a user variable from a tmux pane (`tmux set -pu -t <pane> <name>`).
pub fn clear(pane: &str, name: &str) {
    let _ = Command::new("tmux")
        .args(["set", "-t", pane, "-pu", name])
        .status();
}
