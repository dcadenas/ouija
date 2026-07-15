use std::fs;
use std::path::PathBuf;

fn task_command_lines(task: &str) -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(task);
    let script = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));

    script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn install_tasks_restart_daemon_with_supported_command() {
    for task in ["mise/tasks/use-local", "mise/tasks/use-published"] {
        let commands = task_command_lines(task);

        assert!(
            commands.iter().any(|line| {
                line == "ouija restart-server" || line == "\"$CARGO_BIN\" restart-server"
            }),
            "{task} must restart the daemon through `ouija restart-server`"
        );
        assert!(
            commands.iter().all(|line| !line.contains("ouija start")),
            "{task} must not call the removed `ouija start` subcommand"
        );
        assert!(
            commands.iter().all(|line| !line.contains("ouija stop")),
            "{task} must not call the removed `ouija stop` subcommand"
        );
    }
}

#[test]
fn use_local_updates_the_user_path_copy_and_restarts_the_fresh_binary() {
    let commands = task_command_lines("mise/tasks/use-local");

    assert!(
        commands
            .iter()
            .any(|line| line == "LOCAL_BIN=\"$HOME/.local/bin/ouija\""),
        "use-local must account for ~/.local/bin shadowing ~/.cargo/bin"
    );
    assert!(
        commands
            .iter()
            .any(|line| line == "\"$CARGO_BIN\" restart-server"),
        "use-local must restart through the binary it just built"
    );
}
