use anyhow::{Context, bail};
use fs2::FileExt;
use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_TTL_SECS: i64 = 30 * 60;
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Continuation {
    pub version: u32,
    pub objective: String,
    pub current_slice: String,
    pub confirmed_evidence: Vec<String>,
    pub blockers_decisions: Vec<String>,
    pub next_actions: Vec<String>,
    pub forbidden_scope: Vec<String>,
    pub verification_commands: Vec<String>,
    pub explicitly_known_ouija_descendants: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveCaller {
    pub session_id: String,
    pub incarnation: u64,
    pub binding: StateBinding,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateBinding {
    pub cwd: PathBuf,
    pub git: Option<GitBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBinding {
    pub repository_root: PathBuf,
    pub common_dir: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub dirty_digest: String,
}

pub fn parse_continuation(input: &[u8]) -> anyhow::Result<Continuation> {
    if input.len() > MAX_PAYLOAD_BYTES {
        bail!("continuation payload exceeds 16384 bytes");
    }
    let payload: Continuation = serde_json::from_slice(input)?;
    validate_continuation(&payload)?;
    Ok(payload)
}

fn validate_continuation(_payload: &Continuation) -> anyhow::Result<()> {
    let payload = _payload;
    if payload.version != SCHEMA_VERSION {
        bail!(
            "unsupported continuation version {}; expected {}",
            payload.version,
            SCHEMA_VERSION
        );
    }
    for (name, value) in [
        ("objective", payload.objective.as_str()),
        ("current_slice", payload.current_slice.as_str()),
    ] {
        if value.trim().is_empty() {
            bail!("{name} must not be empty");
        }
    }
    if !(1..=3).contains(&payload.next_actions.len()) {
        bail!("next_actions must contain between one and three actions");
    }
    for (name, values) in [
        ("confirmed_evidence", &payload.confirmed_evidence),
        ("blockers_decisions", &payload.blockers_decisions),
        ("next_actions", &payload.next_actions),
        ("forbidden_scope", &payload.forbidden_scope),
        ("verification_commands", &payload.verification_commands),
        (
            "explicitly_known_ouija_descendants",
            &payload.explicitly_known_ouija_descendants,
        ),
    ] {
        if values.iter().any(|value| value.trim().is_empty()) {
            bail!("{name} must not contain empty entries");
        }
    }
    Ok(())
}

pub fn capture_live_caller(
    session_id: String,
    incarnation: u64,
    cwd: &Path,
) -> anyhow::Result<LiveCaller> {
    if session_id.trim().is_empty() {
        bail!("session id must not be empty");
    }
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("canonicalizing current directory {}", cwd.display()))?;
    let git = capture_git_binding(&cwd)?;
    Ok(LiveCaller {
        session_id,
        incarnation,
        binding: StateBinding { cwd, git },
    })
}

pub fn prepare(
    data_dir: &Path,
    caller: &LiveCaller,
    payload: Continuation,
    replace_expired: bool,
    now: i64,
) -> anyhow::Result<String> {
    validate_continuation(&payload)?;
    let store = Store::open(data_dir, caller)?;
    let _lock = store.lock()?;
    if let Some(existing) = store.read_record()? {
        if existing.state == RecordState::Pending {
            if now <= existing.expires_at {
                bail!(
                    "a pending continuation already exists for session '{}'",
                    caller.session_id
                );
            }
            if !replace_expired {
                bail!("the pending continuation has expired; pass --replace-expired to replace it");
            }
        }
    }

    let token = Alphanumeric.sample_string(&mut rand::rng(), 48);
    let record = Record {
        version: SCHEMA_VERSION,
        token: token.clone(),
        state: RecordState::Pending,
        session_id: caller.session_id.clone(),
        source_incarnation: caller.incarnation.to_string(),
        created_at: now,
        expires_at: now + DEFAULT_TTL_SECS,
        binding: caller.binding.clone(),
        continuation: payload,
        adopted_at: None,
        adopter_incarnation: None,
    };
    store.write_record(&record)?;
    Ok(token)
}

pub fn adopt(
    data_dir: &Path,
    caller: &LiveCaller,
    token: &str,
    now: i64,
) -> anyhow::Result<Continuation> {
    let store = Store::open(data_dir, caller)?;
    let _lock = store.lock()?;
    let mut record = store
        .read_record()?
        .with_context(|| format!("no continuation exists for session '{}'", caller.session_id))?;

    if record.version != SCHEMA_VERSION || record.continuation.version != SCHEMA_VERSION {
        bail!("continuation schema version does not match this Ouija version");
    }
    if record.token != token {
        bail!("continuation token does not match");
    }
    if record.session_id != caller.session_id {
        bail!("continuation belongs to a different session");
    }
    if record.binding != caller.binding {
        bail!("live working state does not match the prepared continuation");
    }

    let source_incarnation = parse_incarnation(
        &record.source_incarnation,
        "continuation source incarnation",
    )?;
    match record.state {
        RecordState::Pending => {
            if now > record.expires_at {
                bail!("continuation has expired");
            }
            if caller.incarnation <= source_incarnation {
                bail!("adoption requires a newer session incarnation");
            }
            record.state = RecordState::Adopted;
            record.adopted_at = Some(now);
            record.adopter_incarnation = Some(caller.incarnation.to_string());
            store.write_record(&record)?;
        }
        RecordState::Adopted => {
            let adopter = record
                .adopter_incarnation
                .as_deref()
                .context("adopted continuation is missing adopter incarnation")
                .and_then(|value| parse_incarnation(value, "adopter incarnation"))?;
            if adopter != caller.incarnation {
                bail!("continuation was already adopted by a different incarnation");
            }
        }
    }
    Ok(record.continuation)
}

pub fn cleanup(
    data_dir: &Path,
    caller: &LiveCaller,
    force_pending: bool,
    now: i64,
) -> anyhow::Result<bool> {
    let store = Store::open(data_dir, caller)?;
    let _lock = store.lock()?;
    let Some(record) = store.read_record()? else {
        return Ok(false);
    };
    if record.state == RecordState::Pending && now <= record.expires_at && !force_pending {
        bail!("continuation is still pending; pass --force-pending to remove it");
    }
    fs::remove_file(&store.record_path)
        .with_context(|| format!("removing {}", store.record_path.display()))?;
    File::open(&store.dir)?.sync_all()?;
    Ok(true)
}

fn parse_incarnation(value: &str, field: &str) -> anyhow::Result<u64> {
    value
        .parse()
        .with_context(|| format!("{field} is not a decimal u64"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordState {
    Pending,
    Adopted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    token: String,
    state: RecordState,
    session_id: String,
    source_incarnation: String,
    created_at: i64,
    expires_at: i64,
    binding: StateBinding,
    continuation: Continuation,
    #[serde(skip_serializing_if = "Option::is_none")]
    adopted_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    adopter_incarnation: Option<String>,
}

struct Store {
    dir: PathBuf,
    record_path: PathBuf,
    lock_path: PathBuf,
}

impl Store {
    fn open(data_dir: &Path, caller: &LiveCaller) -> anyhow::Result<Self> {
        let absolute_data_dir = absolute_path(data_dir)?;
        refuse_repository_local_storage(&absolute_data_dir, &caller.binding)?;
        let dir = absolute_data_dir.join("rollovers");
        create_private_dir(&dir)?;
        let key = session_key(&caller.session_id);
        Ok(Self {
            record_path: dir.join(format!("{key}.json")),
            lock_path: dir.join(format!("{key}.lock")),
            dir,
        })
    }

    fn lock(&self) -> anyhow::Result<File> {
        let file = open_private(&self.lock_path)?;
        file.lock_exclusive()
            .with_context(|| format!("locking {}", self.lock_path.display()))?;
        Ok(file)
    }

    fn read_record(&self) -> anyhow::Result<Option<Record>> {
        let file = match File::open(&self.record_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("opening {}", self.record_path.display()));
            }
        };
        let mut bytes = Vec::new();
        file.take((MAX_PAYLOAD_BYTES * 4 + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_PAYLOAD_BYTES * 4 {
            bail!("stored continuation is unexpectedly large");
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", self.record_path.display()))
            .map(Some)
    }

    fn write_record(&self, record: &Record) -> anyhow::Result<()> {
        let suffix = Alphanumeric.sample_string(&mut rand::rng(), 16);
        let temp_path = self.dir.join(format!(".{suffix}.tmp"));
        let result = (|| -> anyhow::Result<()> {
            let mut temp = create_new_private(&temp_path)?;
            serde_json::to_writer(&mut temp, record)?;
            temp.write_all(b"\n")?;
            temp.sync_all()?;
            fs::rename(&temp_path, &self.record_path).with_context(|| {
                format!(
                    "atomically replacing {} with {}",
                    self.record_path.display(),
                    temp_path.display()
                )
            })?;
            set_private_file_permissions(&self.record_path)?;
            File::open(&self.dir)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }
}

fn absolute_path(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if absolute.exists() {
        return absolute
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", absolute.display()));
    }
    let parent = absolute.parent().context("data directory has no parent")?;
    let name = absolute.file_name().context("data directory has no name")?;
    Ok(absolute_path(parent)?.join(name))
}

fn refuse_repository_local_storage(data_dir: &Path, binding: &StateBinding) -> anyhow::Result<()> {
    let forbidden_roots: Vec<&Path> = match binding.git.as_ref() {
        Some(git) => vec![git.repository_root.as_path(), git.common_dir.as_path()],
        None => vec![binding.cwd.as_path()],
    };
    if let Some(forbidden_root) = forbidden_roots
        .into_iter()
        .find(|root| data_dir.starts_with(root))
    {
        bail!(
            "rollover storage {} is inside working state {}; choose an external Ouija data directory",
            data_dir.display(),
            forbidden_root.display()
        );
    }
    Ok(())
}

fn session_key(session_id: &str) -> String {
    hex_digest(Sha256::digest(session_id.as_bytes()))
}

fn capture_git_binding(cwd: &Path) -> anyhow::Result<Option<GitBinding>> {
    let repository_root = match git_output(cwd, &["rev-parse", "--show-toplevel"])? {
        Some(value) => canonical_existing_path(value.trim(), "repository root")?,
        None => return Ok(None),
    };
    let common_dir = git_required_output(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .and_then(|value| canonical_existing_path(value.trim(), "git common directory"))?;
    let head = git_required_output(cwd, &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let branch = git_output(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"])?
        .map(|value| value.trim().to_string());
    let dirty_digest = git_dirty_digest(&repository_root)?;
    Ok(Some(GitBinding {
        repository_root,
        common_dir,
        branch,
        head,
        dirty_digest,
    }))
}

fn canonical_existing_path(value: &str, description: &str) -> anyhow::Result<PathBuf> {
    Path::new(value)
        .canonicalize()
        .with_context(|| format!("canonicalizing {description} {value}"))
}

fn git_output(cwd: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .context("git returned non-UTF-8 path metadata")
            .map(Some);
    }
    Ok(None)
}

fn git_required_output(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    git_output(cwd, args)?.with_context(|| format!("git {} failed", args.join(" ")))
}

fn git_dirty_digest(repository_root: &Path) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    hash_git_command(
        &mut hasher,
        repository_root,
        "porcelain-v2",
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )?;
    hash_git_command(
        &mut hasher,
        repository_root,
        "unstaged-diff",
        &["diff", "--binary", "--no-ext-diff"],
    )?;
    hash_git_command(
        &mut hasher,
        repository_root,
        "staged-diff",
        &["diff", "--cached", "--binary", "--no-ext-diff"],
    )?;

    let untracked = Command::new("git")
        .arg("-C")
        .arg(repository_root)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .context("listing untracked files")?;
    if !untracked.status.success() {
        bail!("git ls-files failed while fingerprinting untracked files");
    }
    for relative in untracked.stdout.split(|byte| *byte == 0) {
        if relative.is_empty() {
            continue;
        }
        hash_frame(&mut hasher, b"untracked-path", relative);
        let path = repository_root.join(path_from_git_bytes(relative)?);
        hash_untracked_path(&mut hasher, &path)?;
    }
    Ok(hex_digest(hasher.finalize()))
}

fn hash_git_command(
    hasher: &mut Sha256,
    repository_root: &Path,
    label: &str,
    args: &[&str],
) -> anyhow::Result<()> {
    hash_frame(hasher, b"command", label.as_bytes());
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repository_root)
        .args(args)
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    let stdout = child.stdout.take().context("capturing git stdout")?;
    let output_digest = hash_reader_digest(stdout)?;
    let status = child.wait()?;
    if !status.success() {
        bail!("git {} failed while fingerprinting state", args.join(" "));
    }
    hash_frame(hasher, b"command-output-sha256", &output_digest);
    Ok(())
}

fn hash_untracked_path(hasher: &mut Sha256, path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading untracked path {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        hash_frame(hasher, b"type", b"symlink");
        let target = fs::read_link(path)?;
        hash_frame(hasher, b"symlink-target", path_bytes(&target)?);
    } else if file_type.is_file() {
        hash_frame(hasher, b"type", b"file");
        hash_frame(hasher, b"file-size", &metadata.len().to_be_bytes());
        let digest = hash_reader_digest(File::open(path)?)?;
        hash_frame(hasher, b"file-sha256", &digest);
    } else if file_type.is_dir() {
        hash_frame(hasher, b"type", b"directory");
    } else {
        hash_frame(hasher, b"type", b"special");
    }
    Ok(())
}

fn hash_reader_digest(mut reader: impl Read) -> anyhow::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(unix)]
fn path_from_git_bytes(bytes: &[u8]) -> anyhow::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn path_from_git_bytes(bytes: &[u8]) -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(std::str::from_utf8(bytes)?))
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> anyhow::Result<&[u8]> {
    use std::os::unix::ffi::OsStrExt;
    Ok(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> anyhow::Result<&[u8]> {
    path.to_str()
        .map(str::as_bytes)
        .context("path is not UTF-8")
}

fn hash_frame(hasher: &mut Sha256, label: &[u8], bytes: &[u8]) {
    hasher.update((label.len() as u64).to_be_bytes());
    hasher.update(label);
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let mut output = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))
}

#[cfg(unix)]
fn open_private(path: &Path) -> anyhow::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    set_private_file_permissions(path)?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_private(path: &Path) -> anyhow::Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?)
}

#[cfg(unix)]
fn create_new_private(path: &Path) -> anyhow::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?)
}

#[cfg(not(unix))]
fn create_new_private(path: &Path) -> anyhow::Result<File> {
    Ok(OpenOptions::new().create_new(true).write(true).open(path)?)
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn payload() -> Continuation {
        Continuation {
            version: SCHEMA_VERSION,
            objective: "Finish context rollover".into(),
            current_slice: "Implement the local helper".into(),
            confirmed_evidence: vec!["HEAD is abc".into()],
            blockers_decisions: vec!["Records are disposable".into()],
            next_actions: vec!["Run focused tests".into()],
            forbidden_scope: vec!["Do not push".into()],
            verification_commands: vec!["cargo test rollover".into()],
            explicitly_known_ouija_descendants: vec!["worker".into()],
        }
    }

    fn init_git(path: &Path) {
        fs::create_dir_all(path).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(path)
                .status()
                .unwrap()
                .success()
        );
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(path)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .status()
            .unwrap();
        fs::write(path.join("tracked.txt"), "base\n").unwrap();
        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(path)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-qm", "base"])
            .current_dir(path)
            .status()
            .unwrap();
    }

    #[test]
    fn continuation_schema_is_bounded_and_requires_one_to_three_actions() {
        assert!(parse_continuation(&serde_json::to_vec(&payload()).unwrap()).is_ok());

        let mut invalid = payload();
        invalid.next_actions.clear();
        assert!(
            parse_continuation(&serde_json::to_vec(&invalid).unwrap())
                .unwrap_err()
                .to_string()
                .contains("next_actions")
        );

        let mut invalid = payload();
        invalid.next_actions = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        assert!(
            parse_continuation(&serde_json::to_vec(&invalid).unwrap())
                .unwrap_err()
                .to_string()
                .contains("next_actions")
        );

        let oversized = vec![b' '; MAX_PAYLOAD_BYTES + 1];
        assert!(
            parse_continuation(&oversized)
                .unwrap_err()
                .to_string()
                .contains("16384")
        );
    }

    #[test]
    fn fingerprint_detects_tracked_staged_and_untracked_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        init_git(dir.path());
        let clean = capture_live_caller("worker".into(), 1, dir.path()).unwrap();

        fs::write(dir.path().join("tracked.txt"), "unstaged\n").unwrap();
        let unstaged = capture_live_caller("worker".into(), 1, dir.path()).unwrap();
        assert_ne!(clean.binding, unstaged.binding);

        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        let staged = capture_live_caller("worker".into(), 1, dir.path()).unwrap();
        assert_ne!(unstaged.binding, staged.binding);

        fs::write(dir.path().join("new.txt"), "one\n").unwrap();
        let untracked_one = capture_live_caller("worker".into(), 1, dir.path()).unwrap();
        fs::write(dir.path().join("new.txt"), "two\n").unwrap();
        let untracked_two = capture_live_caller("worker".into(), 1, dir.path()).unwrap();
        assert_ne!(untracked_one.binding, untracked_two.binding);
    }

    #[test]
    fn pending_adoption_is_bound_and_idempotent() {
        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        init_git(repo.path());
        let source = capture_live_caller("worker".into(), 7, repo.path()).unwrap();
        let token = prepare(data.path(), &source, payload(), false, 100).unwrap();

        let same = capture_live_caller("worker".into(), 7, repo.path()).unwrap();
        assert!(
            adopt(data.path(), &same, &token, 101)
                .unwrap_err()
                .to_string()
                .contains("newer")
        );

        let adopter = capture_live_caller("worker".into(), 8, repo.path()).unwrap();
        assert_eq!(
            adopt(data.path(), &adopter, &token, 101).unwrap(),
            payload()
        );
        assert_eq!(
            adopt(data.path(), &adopter, &token, 102).unwrap(),
            payload()
        );

        let newer = capture_live_caller("worker".into(), 9, repo.path()).unwrap();
        assert!(adopt(data.path(), &newer, &token, 102).is_err());
        assert!(prepare(data.path(), &adopter, payload(), false, 103).is_ok());
    }

    #[test]
    fn refusals_do_not_mutate_pending_record() {
        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        init_git(repo.path());
        let source = capture_live_caller("worker".into(), 1, repo.path()).unwrap();
        let token = prepare(data.path(), &source, payload(), false, 100).unwrap();
        let record_path = record_path_for_test(data.path(), "worker");
        let before = fs::read(&record_path).unwrap();

        let wrong = capture_live_caller("other".into(), 2, repo.path()).unwrap();
        assert!(adopt(data.path(), &wrong, &token, 101).is_err());
        assert_eq!(fs::read(&record_path).unwrap(), before);

        let adopter = capture_live_caller("worker".into(), 2, repo.path()).unwrap();
        assert!(adopt(data.path(), &adopter, "wrong-token", 101).is_err());
        assert_eq!(fs::read(&record_path).unwrap(), before);

        fs::write(repo.path().join("tracked.txt"), "changed\n").unwrap();
        let changed = capture_live_caller("worker".into(), 2, repo.path()).unwrap();
        assert!(adopt(data.path(), &changed, &token, 101).is_err());
        assert_eq!(fs::read(&record_path).unwrap(), before);

        assert!(adopt(data.path(), &adopter, &token, 100 + DEFAULT_TTL_SECS + 1).is_err());
        assert_eq!(fs::read(&record_path).unwrap(), before);
    }

    #[test]
    fn expired_pending_requires_explicit_replacement() {
        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        init_git(repo.path());
        let caller = capture_live_caller("worker".into(), 1, repo.path()).unwrap();
        prepare(data.path(), &caller, payload(), false, 100).unwrap();
        let expired = 100 + DEFAULT_TTL_SECS + 1;
        assert!(prepare(data.path(), &caller, payload(), false, expired).is_err());
        assert!(prepare(data.path(), &caller, payload(), true, expired).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn storage_is_private_and_refuses_repository_local_data_dir() {
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        init_git(repo.path());
        let caller = capture_live_caller("worker".into(), 1, repo.path()).unwrap();
        prepare(data.path(), &caller, payload(), false, 100).unwrap();
        let rollovers = data.path().join("rollovers");
        assert_eq!(
            fs::metadata(&rollovers).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(record_path_for_test(data.path(), "worker"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let repository_local = repo.path().join(".local-state");
        assert!(prepare(&repository_local, &caller, payload(), false, 100).is_err());
        assert!(!repository_local.exists());
    }

    #[test]
    fn non_git_binding_requires_the_exact_canonical_cwd() {
        let root = tempfile::tempdir().unwrap();
        let one = root.path().join("one");
        let two = root.path().join("two");
        fs::create_dir_all(&one).unwrap();
        fs::create_dir_all(&two).unwrap();
        let first = capture_live_caller("worker".into(), 1, &one).unwrap();
        let second = capture_live_caller("worker".into(), 2, &two).unwrap();
        assert!(first.binding.git.is_none());
        assert_ne!(first.binding, second.binding);
    }

    #[test]
    fn adoption_refuses_branch_and_head_changes_without_mutating_record() {
        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        init_git(repo.path());
        let source = capture_live_caller("worker".into(), 1, repo.path()).unwrap();
        let token = prepare(data.path(), &source, payload(), false, 100).unwrap();
        let record_path = record_path_for_test(data.path(), "worker");
        let before = fs::read(&record_path).unwrap();

        Command::new("git")
            .args(["switch", "-qc", "other"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        fs::write(repo.path().join("tracked.txt"), "other\n").unwrap();
        Command::new("git")
            .args(["commit", "-qam", "other"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        let changed = capture_live_caller("worker".into(), 2, repo.path()).unwrap();
        assert!(adopt(data.path(), &changed, &token, 101).is_err());
        assert_eq!(fs::read(record_path).unwrap(), before);
    }

    #[test]
    fn concurrent_prepare_serializes_to_one_pending_record() {
        use std::sync::{Arc, Barrier};

        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        init_git(repo.path());
        let caller = Arc::new(capture_live_caller("worker".into(), 1, repo.path()).unwrap());
        let barrier = Arc::new(Barrier::new(2));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let caller = caller.clone();
            let barrier = barrier.clone();
            let data_dir = data.path().to_path_buf();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                prepare(&data_dir, &caller, payload(), false, 100)
            }));
        }
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);

        let bytes = fs::read(record_path_for_test(data.path(), "worker")).unwrap();
        let record: Record = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record.state, RecordState::Pending);
    }

    #[test]
    fn cleanup_refuses_live_pending_without_an_explicit_override() {
        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        init_git(repo.path());
        let caller = capture_live_caller("worker".into(), 1, repo.path()).unwrap();
        prepare(data.path(), &caller, payload(), false, 100).unwrap();

        assert!(cleanup(data.path(), &caller, false, 101).is_err());
        assert!(record_path_for_test(data.path(), "worker").exists());
        assert!(cleanup(data.path(), &caller, true, 101).unwrap());
        assert!(!record_path_for_test(data.path(), "worker").exists());
        assert!(!cleanup(data.path(), &caller, false, 101).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn storage_path_symlinked_into_repository_is_refused_before_creation() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        init_git(repo.path());
        let link = outside.path().join("data-link");
        symlink(repo.path(), &link).unwrap();
        let caller = capture_live_caller("worker".into(), 1, repo.path()).unwrap();
        assert!(prepare(&link, &caller, payload(), false, 100).is_err());
        assert!(!repo.path().join("rollovers").exists());
    }

    fn record_path_for_test(data_dir: &Path, session_id: &str) -> PathBuf {
        data_dir
            .join("rollovers")
            .join(format!("{}.json", session_key(session_id)))
    }
}
