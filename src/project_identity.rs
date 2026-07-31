use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Identifies the live worktree and its stable repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectIdentity {
    pub project_dir: String,
    pub canonical_repository: String,
}

/// Reports an unusable project path.
#[derive(Debug)]
pub(crate) struct ProjectIdentityError {
    message: String,
}

impl fmt::Display for ProjectIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProjectIdentityError {}

/// Resolve one live directory into worktree and repository identities.
pub(crate) fn resolve_project_identity(
    path: &str,
) -> Result<ProjectIdentity, ProjectIdentityError> {
    let normalized = normalize_absolute(Path::new(path))?;
    let fallback = normalized.to_string_lossy().into_owned();
    let Some(worktree) = git_path(&normalized, "--show-toplevel") else {
        return Ok(ProjectIdentity {
            project_dir: fallback.clone(),
            canonical_repository: fallback,
        });
    };
    let Some(common_dir) = git_path(&normalized, "--git-common-dir") else {
        return Ok(ProjectIdentity {
            project_dir: fallback.clone(),
            canonical_repository: fallback,
        });
    };
    let worktree = normalize_absolute(&worktree)?;
    let common_dir = normalize_absolute(&common_dir)?;
    let canonical_repository = if common_dir.file_name() == Some(OsStr::new(".git")) {
        common_dir
            .parent()
            .ok_or_else(|| ProjectIdentityError {
                message: format!(
                    "git common directory '{}' has no repository parent",
                    common_dir.display()
                ),
            })?
            .to_path_buf()
    } else {
        common_dir
    };

    Ok(ProjectIdentity {
        project_dir: worktree.to_string_lossy().into_owned(),
        canonical_repository: canonical_repository.to_string_lossy().into_owned(),
    })
}

/// Resolve project identity without blocking an async runtime worker.
pub(crate) async fn resolve_project_identity_async(
    path: &str,
) -> Result<ProjectIdentity, ProjectIdentityError> {
    let path = path.to_string();
    tokio::task::spawn_blocking(move || resolve_project_identity(&path))
        .await
        .map_err(|error| ProjectIdentityError {
            message: format!("project identity task failed: {error}"),
        })?
}

fn git_path(cwd: &Path, field: &str) -> Option<PathBuf> {
    let output = Command::new("git")
        .args([
            "-C",
            cwd.to_str()?,
            "rev-parse",
            "--path-format=absolute",
            field,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, ProjectIdentityError> {
    if !path.is_absolute() {
        return Err(ProjectIdentityError {
            message: format!("project path '{}' is not absolute", path.display()),
        });
    }
    if path.parent().is_none() {
        return Err(ProjectIdentityError {
            message: "filesystem root is not a usable project".into(),
        });
    }
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }

    let mut existing = path;
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| ProjectIdentityError {
                message: format!("project path '{}' cannot be normalized", path.display()),
            })?
            .to_os_string();
        missing.push(name);
        existing = existing.parent().ok_or_else(|| ProjectIdentityError {
            message: format!("project path '{}' has no existing ancestor", path.display()),
        })?;
    }
    let mut normalized = existing
        .canonicalize()
        .map_err(|error| ProjectIdentityError {
            message: format!("failed to normalize '{}': {error}", path.display()),
        })?;
    for component in missing.into_iter().rev() {
        normalized.push(component);
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::resolve_project_identity;

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git must start");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn initialized_repository() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        git(temp.path(), &["init", "-q"]);
        git(
            temp.path(),
            &["config", "user.email", "tests@example.invalid"],
        );
        git(temp.path(), &["config", "user.name", "Ouija Tests"]);
        git(temp.path(), &["commit", "--allow-empty", "-qm", "initial"]);
        temp
    }

    #[test]
    fn normal_repository_uses_top_level_for_both_identities() {
        let repository = initialized_repository();
        let nested = repository.path().join("nested");
        std::fs::create_dir(&nested).expect("nested directory");

        let identity = resolve_project_identity(nested.to_str().expect("utf8 path"))
            .expect("project identity");

        let expected = repository.path().canonicalize().expect("canonical repo");
        assert_eq!(identity.project_dir, expected.to_string_lossy());
        assert_eq!(identity.canonical_repository, expected.to_string_lossy());
    }

    #[test]
    fn rootfix_linked_worktree_keeps_worktree_and_common_repository() {
        let home = tempfile::tempdir().expect("home");
        let repository = home.path().join("code/ouija");
        std::fs::create_dir_all(&repository).expect("repository parent");
        git(&repository, &["init", "-q"]);
        git(
            &repository,
            &["config", "user.email", "tests@example.invalid"],
        );
        git(&repository, &["config", "user.name", "Ouija Tests"]);
        git(&repository, &["commit", "--allow-empty", "-qm", "initial"]);
        let worktree = home.path().join(".ouija/worktrees/ouija/rootfix");
        std::fs::create_dir_all(worktree.parent().expect("worktree parent"))
            .expect("worktree parent");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "rootfix-test",
                worktree.to_str().expect("utf8 path"),
            ],
        );

        let identity = resolve_project_identity(worktree.to_str().expect("utf8 path"))
            .expect("project identity");

        assert_eq!(
            identity.project_dir,
            worktree.canonicalize().expect("worktree").to_string_lossy()
        );
        assert_eq!(
            identity.canonical_repository,
            repository
                .canonicalize()
                .expect("repository")
                .to_string_lossy()
        );
        assert_ne!(
            identity.project_dir,
            home.path().canonicalize().expect("home").to_string_lossy()
        );
    }

    #[test]
    fn separate_common_directory_is_the_canonical_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let worktree = temp.path().join("worktree");
        let common = temp.path().join("git-common");
        std::fs::create_dir(&worktree).expect("worktree");
        let output = Command::new("git")
            .args([
                "init",
                "-q",
                "--separate-git-dir",
                common.to_str().expect("utf8 path"),
                worktree.to_str().expect("utf8 path"),
            ])
            .output()
            .expect("git init");
        assert!(output.status.success());

        let identity = resolve_project_identity(worktree.to_str().expect("utf8 path"))
            .expect("project identity");

        assert_eq!(
            identity.canonical_repository,
            common.canonicalize().expect("common").to_string_lossy()
        );
    }

    #[test]
    fn git_failure_preserves_the_full_normalized_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nested = temp.path().join(".ouija/worktrees/ouija/rootfix");
        std::fs::create_dir_all(&nested).expect("nested");

        let identity = resolve_project_identity(nested.to_str().expect("utf8 path"))
            .expect("fallback identity");
        let expected = nested.canonicalize().expect("canonical nested");

        assert_eq!(identity.project_dir, expected.to_string_lossy());
        assert_eq!(identity.canonical_repository, expected.to_string_lossy());
        assert_ne!(
            identity.project_dir,
            temp.path()
                .canonicalize()
                .expect("tempdir")
                .to_string_lossy()
        );
    }
}
