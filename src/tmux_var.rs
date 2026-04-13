use std::process::Command;

const VAR_NAME: &str = "@ouija_session";

/// Set the `@ouija_session` user variable on a tmux pane.
pub fn set(pane: &str, session_id: &str) {
    let _ = Command::new("tmux")
        .args(["set", "-t", pane, "-p", VAR_NAME, session_id])
        .status();
}

/// Read the `@ouija_session` user variable from a tmux pane.
pub fn get(pane: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["display", "-p", "-t", pane, "#{@ouija_session}"])
        .output()
        .ok()?;
    let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// Clear the `@ouija_session` user variable from a tmux pane.
pub fn clear(pane: &str) {
    let _ = Command::new("tmux")
        .args(["set", "-t", pane, "-pu", VAR_NAME])
        .status();
}
