use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::protocol::{PinnedSha, RunId, TaskId};

const REGISTRY_VERSION: u16 = 1;
const CONTROL_DIR: &str = ".rtrt-orchestrator-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeStatus {
    Active,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorktreeKey {
    pub run_id: RunId,
    pub task_id: TaskId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWorktree {
    pub key: WorktreeKey,
    pub path: PathBuf,
    pub base_sha: PinnedSha,
    pub head_sha: PinnedSha,
    pub status: WorktreeStatus,
    pub dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreservationReason {
    Active,
    Dirty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservedWorktree {
    pub key: WorktreeKey,
    pub status: WorktreeStatus,
    pub reason: PreservationReason,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub removed: Vec<WorktreeKey>,
    pub preserved: Vec<PreservedWorktree>,
}

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("failed to {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("Git {operation} failed: {stderr}")]
    Git {
        operation: &'static str,
        stderr: String,
    },
    #[error("Git {operation} returned non-UTF-8 output")]
    NonUtf8GitOutput { operation: &'static str },
    #[error("repository path must be the canonical Git worktree root: {0}")]
    NotRepositoryRoot(PathBuf),
    #[error("owned root must not be inside the source repository")]
    OwnedRootInsideRepository,
    #[error("unsafe manager control path: {0}")]
    UnsafeControlPath(PathBuf),
    #[error("base commit does not resolve to the pinned SHA {0}")]
    BaseCommitMismatch(PinnedSha),
    #[error("repository config contains executable Git filters: {0}")]
    ExecutableGitFilters(String),
    #[error("worktree is already registered: {0:?}")]
    AlreadyManaged(WorktreeKey),
    #[error("worktree is not registered: {0:?}")]
    NotManaged(WorktreeKey),
    #[error("worktree target already exists: {0}")]
    TargetExists(PathBuf),
    #[error("registered worktree is missing: {0}")]
    WorktreeMissing(PathBuf),
    #[error("registered worktree path is unsafe: {0}")]
    UnsafeWorktreePath(PathBuf),
    #[error("registered worktree belongs to another Git repository: {0}")]
    ForeignWorktree(PathBuf),
    #[error("refusing to remove dirty worktree: {0}")]
    DirtyWorktree(PathBuf),
    #[error("invalid worktree registry: {0}")]
    InvalidRegistry(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    version: u16,
    run_id: RunId,
    task_id: TaskId,
    base_sha: PinnedSha,
    status: WorktreeStatus,
}

impl Registration {
    fn key(&self) -> WorktreeKey {
        WorktreeKey {
            run_id: self.run_id.clone(),
            task_id: self.task_id.clone(),
        }
    }
}

/// Owns detached worktrees rooted at one canonical repository and one owned directory.
#[derive(Debug)]
pub struct WorktreeManager {
    repo_root: PathBuf,
    owned_root: PathBuf,
    registrations_root: PathBuf,
    disabled_hooks: PathBuf,
    empty_global_config: PathBuf,
    common_git_dir: PathBuf,
}

impl WorktreeManager {
    pub fn new(
        repo_root: impl AsRef<Path>,
        owned_root: impl AsRef<Path>,
    ) -> Result<Self, WorktreeError> {
        let repo_root = canonicalize(repo_root.as_ref(), "canonicalize repository root")?;
        fs::create_dir_all(owned_root.as_ref()).map_err(|source| WorktreeError::Io {
            operation: "create owned root",
            source,
        })?;
        let owned_root = canonicalize(owned_root.as_ref(), "canonicalize owned root")?;
        if owned_root.starts_with(&repo_root) {
            return Err(WorktreeError::OwnedRootInsideRepository);
        }

        let control_root = prepare_directory(&owned_root.join(CONTROL_DIR), &owned_root)?;
        let registrations_root =
            prepare_directory(&control_root.join("registrations"), &control_root)?;
        let disabled_hooks = prepare_empty_file(&control_root.join("disabled-hooks"))?;
        let empty_global_config = prepare_empty_file(&control_root.join("empty-gitconfig"))?;

        let mut manager = Self {
            repo_root,
            owned_root,
            registrations_root,
            disabled_hooks,
            empty_global_config,
            common_git_dir: PathBuf::new(),
        };

        let reported_root = manager.git_path_output(
            &manager.repo_root,
            ["rev-parse", "--show-toplevel"],
            "inspect repository root",
        )?;
        if reported_root != manager.repo_root {
            return Err(WorktreeError::NotRepositoryRoot(manager.repo_root));
        }
        manager.ensure_no_executable_filters(&manager.repo_root)?;
        manager.common_git_dir = manager.git_path_output(
            &manager.repo_root,
            ["rev-parse", "--git-common-dir"],
            "inspect common Git directory",
        )?;
        Ok(manager)
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn owned_root(&self) -> &Path {
        &self.owned_root
    }

    pub fn add(
        &self,
        run_id: RunId,
        task_id: TaskId,
        base_sha: PinnedSha,
    ) -> Result<ManagedWorktree, WorktreeError> {
        self.ensure_no_executable_filters(&self.repo_root)?;
        self.verify_base_commit(&base_sha)?;
        let registration = Registration {
            version: REGISTRY_VERSION,
            run_id,
            task_id,
            base_sha,
            status: WorktreeStatus::Active,
        };
        let key = registration.key();
        let registration_path = self.registration_path(&key);
        if registration_path.exists() {
            return Err(WorktreeError::AlreadyManaged(key));
        }

        let target = self.worktree_path(&key);
        if fs::symlink_metadata(&target).is_ok() {
            return Err(WorktreeError::TargetExists(target));
        }

        let output = self
            .git_command(&self.repo_root)
            .args(["worktree", "add", "--detach", "--"])
            .arg(&target)
            .arg(registration.base_sha.as_str())
            .output()
            .map_err(|source| WorktreeError::Io {
                operation: "start Git worktree add",
                source,
            })?;
        ensure_git_success(output, "worktree add")?;

        let managed = match self.inspect(&registration) {
            Ok(managed) => managed,
            Err(error) => {
                self.rollback_add(&target);
                return Err(error);
            }
        };
        if let Err(error) = self.create_registration(&registration) {
            self.rollback_add(&target);
            return Err(error);
        }
        Ok(managed)
    }

    pub fn list(&self) -> Result<Vec<ManagedWorktree>, WorktreeError> {
        let registrations = self.load_registrations()?;
        let mut worktrees = registrations
            .iter()
            .map(|registration| self.inspect(registration))
            .collect::<Result<Vec<_>, _>>()?;
        worktrees.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(worktrees)
    }

    pub fn set_status(
        &self,
        run_id: &RunId,
        task_id: &TaskId,
        status: WorktreeStatus,
    ) -> Result<(), WorktreeError> {
        let key = WorktreeKey {
            run_id: run_id.clone(),
            task_id: task_id.clone(),
        };
        let mut registration = self.load_registration(&key)?;
        registration.status = status;
        self.replace_registration(&registration)
    }

    /// Removes one registered worktree only when it is still clean and owned.
    pub fn remove(
        &self,
        run_id: &RunId,
        task_id: &TaskId,
    ) -> Result<ManagedWorktree, WorktreeError> {
        let key = WorktreeKey {
            run_id: run_id.clone(),
            task_id: task_id.clone(),
        };
        let registration = self.load_registration(&key)?;
        let managed = self.inspect(&registration)?;
        if managed.dirty {
            return Err(WorktreeError::DirtyWorktree(managed.path));
        }

        let output = self
            .git_command(&self.repo_root)
            .args(["worktree", "remove", "--"])
            .arg(&managed.path)
            .output()
            .map_err(|source| WorktreeError::Io {
                operation: "start Git worktree remove",
                source,
            })?;
        ensure_git_success(output, "worktree remove")?;
        self.delete_registration(&key)?;
        Ok(managed)
    }

    /// Removes clean terminal worktrees. Active and all dirty worktrees are retained.
    pub fn cleanup(&self) -> Result<CleanupReport, WorktreeError> {
        let mut report = CleanupReport::default();
        for managed in self.list()? {
            let reason = if managed.status == WorktreeStatus::Active {
                Some(PreservationReason::Active)
            } else if managed.dirty {
                Some(PreservationReason::Dirty)
            } else {
                None
            };

            if let Some(reason) = reason {
                report.preserved.push(PreservedWorktree {
                    key: managed.key,
                    status: managed.status,
                    reason,
                });
            } else {
                let key = managed.key;
                match self.remove(&key.run_id, &key.task_id) {
                    Ok(_) => report.removed.push(key),
                    Err(WorktreeError::DirtyWorktree(_)) => {
                        report.preserved.push(PreservedWorktree {
                            key,
                            status: managed.status,
                            reason: PreservationReason::Dirty,
                        });
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(report)
    }

    fn inspect(&self, registration: &Registration) -> Result<ManagedWorktree, WorktreeError> {
        let key = registration.key();
        let expected = self.worktree_path(&key);
        let metadata = fs::symlink_metadata(&expected).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                WorktreeError::WorktreeMissing(expected.clone())
            } else {
                WorktreeError::Io {
                    operation: "inspect worktree path",
                    source,
                }
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(WorktreeError::UnsafeWorktreePath(expected));
        }
        let path = canonicalize(&expected, "canonicalize worktree")?;
        if path != expected || !path.starts_with(&self.owned_root) {
            return Err(WorktreeError::UnsafeWorktreePath(path));
        }

        let reported_root = self.git_path_output(
            &path,
            ["rev-parse", "--show-toplevel"],
            "inspect worktree root",
        )?;
        let common_git_dir = self.git_path_output(
            &path,
            ["rev-parse", "--git-common-dir"],
            "inspect worktree common Git directory",
        )?;
        if reported_root != path || common_git_dir != self.common_git_dir {
            return Err(WorktreeError::ForeignWorktree(path));
        }
        self.ensure_no_executable_filters(&path)?;

        let head = self.git_text_output(&path, ["rev-parse", "HEAD"], "inspect worktree HEAD")?;
        let head_sha = PinnedSha::new(head)
            .map_err(|error| WorktreeError::InvalidRegistry(error.to_string()))?;
        let status = self.git_output(
            &path,
            ["status", "--porcelain=v1", "--untracked-files=normal"],
            "inspect worktree status",
        )?;

        Ok(ManagedWorktree {
            key,
            path,
            base_sha: registration.base_sha.clone(),
            head_sha,
            status: registration.status,
            dirty: !status.stdout.is_empty(),
        })
    }

    fn verify_base_commit(&self, base_sha: &PinnedSha) -> Result<(), WorktreeError> {
        let expression = format!("{}^{{commit}}", base_sha.as_str());
        let resolved = self.git_text_output(
            &self.repo_root,
            [
                "rev-parse",
                "--verify",
                "--end-of-options",
                expression.as_str(),
            ],
            "verify base commit",
        )?;
        if resolved == base_sha.as_str() {
            Ok(())
        } else {
            Err(WorktreeError::BaseCommitMismatch(base_sha.clone()))
        }
    }

    fn ensure_no_executable_filters(&self, cwd: &Path) -> Result<(), WorktreeError> {
        let output = self
            .git_command(cwd)
            .args([
                "config",
                "--includes",
                "--name-only",
                "--get-regexp",
                "^filter\\..*\\.(clean|smudge|process)$",
            ])
            .output()
            .map_err(|source| WorktreeError::Io {
                operation: "inspect Git filter configuration",
                source,
            })?;
        if output.status.success() {
            let keys =
                String::from_utf8(output.stdout).map_err(|_| WorktreeError::NonUtf8GitOutput {
                    operation: "inspect Git filter configuration",
                })?;
            let keys = keys.trim();
            if keys.is_empty() {
                Ok(())
            } else {
                Err(WorktreeError::ExecutableGitFilters(keys.to_owned()))
            }
        } else if output.status.code() == Some(1) {
            // `git config --get-regexp` uses status 1 when no keys match.
            Ok(())
        } else {
            ensure_git_success(output, "inspect Git filter configuration").map(drop)
        }
    }

    fn load_registrations(&self) -> Result<Vec<Registration>, WorktreeError> {
        let entries =
            fs::read_dir(&self.registrations_root).map_err(|source| WorktreeError::Io {
                operation: "read worktree registrations",
                source,
            })?;
        let mut registrations = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| WorktreeError::Io {
                operation: "read worktree registration entry",
                source,
            })?;
            let path = entry.path();
            if path.extension() != Some(OsStr::new("json")) {
                continue;
            }
            let registration = self.read_registration_file(&path)?;
            if path != self.registration_path(&registration.key()) {
                return Err(WorktreeError::InvalidRegistry(format!(
                    "registration filename does not match its IDs: {}",
                    path.display()
                )));
            }
            registrations.push(registration);
        }
        Ok(registrations)
    }

    fn load_registration(&self, key: &WorktreeKey) -> Result<Registration, WorktreeError> {
        let path = self.registration_path(key);
        if !path.exists() {
            return Err(WorktreeError::NotManaged(key.clone()));
        }
        let registration = self.read_registration_file(&path)?;
        if registration.key() != *key {
            return Err(WorktreeError::InvalidRegistry(format!(
                "registration IDs do not match {}",
                path.display()
            )));
        }
        Ok(registration)
    }

    fn read_registration_file(&self, path: &Path) -> Result<Registration, WorktreeError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| WorktreeError::Io {
            operation: "inspect worktree registration",
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(WorktreeError::UnsafeControlPath(path.to_path_buf()));
        }
        let file = File::open(path).map_err(|source| WorktreeError::Io {
            operation: "open worktree registration",
            source,
        })?;
        let registration: Registration = serde_json::from_reader(file)
            .map_err(|error| WorktreeError::InvalidRegistry(error.to_string()))?;
        if registration.version != REGISTRY_VERSION {
            return Err(WorktreeError::InvalidRegistry(format!(
                "unsupported registry version {}",
                registration.version
            )));
        }
        Ok(registration)
    }

    fn create_registration(&self, registration: &Registration) -> Result<(), WorktreeError> {
        let path = self.registration_path(&registration.key());
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| {
                if source.kind() == io::ErrorKind::AlreadyExists {
                    WorktreeError::AlreadyManaged(registration.key())
                } else {
                    WorktreeError::Io {
                        operation: "create worktree registration",
                        source,
                    }
                }
            })?;
        write_registration(file, registration)
    }

    fn replace_registration(&self, registration: &Registration) -> Result<(), WorktreeError> {
        let path = self.registration_path(&registration.key());
        let metadata = fs::symlink_metadata(&path).map_err(|source| WorktreeError::Io {
            operation: "inspect worktree registration",
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(WorktreeError::UnsafeControlPath(path));
        }
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|source| WorktreeError::Io {
                operation: "update worktree registration",
                source,
            })?;
        write_registration(file, registration)
    }

    fn delete_registration(&self, key: &WorktreeKey) -> Result<(), WorktreeError> {
        let path = self.registration_path(key);
        let metadata = fs::symlink_metadata(&path).map_err(|source| WorktreeError::Io {
            operation: "inspect worktree registration",
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(WorktreeError::UnsafeControlPath(path));
        }
        fs::remove_file(path).map_err(|source| WorktreeError::Io {
            operation: "delete worktree registration",
            source,
        })
    }

    fn registration_path(&self, key: &WorktreeKey) -> PathBuf {
        self.registrations_root
            .join(format!("{}.json", encoded_key(key)))
    }

    fn worktree_path(&self, key: &WorktreeKey) -> PathBuf {
        self.owned_root.join(format!("wt-{}", encoded_key(key)))
    }

    fn rollback_add(&self, target: &Path) {
        let _ = self
            .git_command(&self.repo_root)
            .args(["worktree", "remove", "--"])
            .arg(target)
            .output();
    }

    fn git_command(&self, cwd: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &self.empty_global_config)
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_0", &self.disabled_hooks)
            .env("GIT_CONFIG_KEY_1", "core.fsmonitor")
            .env("GIT_CONFIG_VALUE_1", "false")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_CONFIG")
            .env_remove("GIT_CONFIG_SYSTEM")
            .env_remove("GIT_EXEC_PATH")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
        command
    }

    fn git_output<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        operation: &'static str,
    ) -> Result<Output, WorktreeError> {
        let output = self
            .git_command(cwd)
            .args(args)
            .output()
            .map_err(|source| WorktreeError::Io {
                operation: "start Git",
                source,
            })?;
        ensure_git_success(output, operation)
    }

    fn git_text_output<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        operation: &'static str,
    ) -> Result<String, WorktreeError> {
        let output = self.git_output(cwd, args, operation)?;
        let text = String::from_utf8(output.stdout)
            .map_err(|_| WorktreeError::NonUtf8GitOutput { operation })?;
        Ok(text.trim_end_matches(['\r', '\n'].as_slice()).to_owned())
    }

    fn git_path_output<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        operation: &'static str,
    ) -> Result<PathBuf, WorktreeError> {
        let value = self.git_text_output(cwd, args, operation)?;
        let path = Path::new(&value);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        canonicalize(&path, operation)
    }
}

fn encoded_key(key: &WorktreeKey) -> String {
    format!(
        "r{}-{}-t{}-{}",
        key.run_id.as_str().len(),
        key.run_id,
        key.task_id.as_str().len(),
        key.task_id
    )
}

fn prepare_directory(path: &Path, expected_parent: &Path) -> Result<PathBuf, WorktreeError> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(WorktreeError::UnsafeControlPath(path.to_path_buf()));
        }
    }
    fs::create_dir_all(path).map_err(|source| WorktreeError::Io {
        operation: "create manager control directory",
        source,
    })?;
    let canonical = canonicalize(path, "canonicalize manager control directory")?;
    if canonical.parent() != Some(expected_parent) {
        return Err(WorktreeError::UnsafeControlPath(canonical));
    }
    Ok(canonical)
}

fn prepare_empty_file(path: &Path) -> Result<PathBuf, WorktreeError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file.sync_all().map_err(|source| WorktreeError::Io {
            operation: "sync manager control file",
            source,
        })?,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).map_err(|source| WorktreeError::Io {
                operation: "inspect manager control file",
                source,
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != 0 {
                return Err(WorktreeError::UnsafeControlPath(path.to_path_buf()));
            }
        }
        Err(source) => {
            return Err(WorktreeError::Io {
                operation: "create manager control file",
                source,
            });
        }
    }
    canonicalize(path, "canonicalize manager control file")
}

fn write_registration(mut file: File, registration: &Registration) -> Result<(), WorktreeError> {
    let encoded = serde_json::to_vec(registration)
        .map_err(|error| WorktreeError::InvalidRegistry(error.to_string()))?;
    file.write_all(&encoded)
        .map_err(|source| WorktreeError::Io {
            operation: "write worktree registration",
            source,
        })?;
    file.sync_all().map_err(|source| WorktreeError::Io {
        operation: "sync worktree registration",
        source,
    })
}

fn canonicalize(path: &Path, operation: &'static str) -> Result<PathBuf, WorktreeError> {
    path.canonicalize()
        .map_err(|source| WorktreeError::Io { operation, source })
}

fn ensure_git_success(output: Output, operation: &'static str) -> Result<Output, WorktreeError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(WorktreeError::Git {
            operation,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        _temp: TempDir,
        repo: PathBuf,
        owned: PathBuf,
        first_sha: PinnedSha,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            let owned = temp.path().join("owned");
            fs::create_dir(&repo).unwrap();
            git(&repo, ["init", "--quiet"]);
            git(&repo, ["config", "user.name", "RTRT Test"]);
            git(&repo, ["config", "user.email", "rtrt@example.invalid"]);
            fs::write(repo.join("tracked.txt"), "first\n").unwrap();
            git(&repo, ["add", "tracked.txt"]);
            git(
                &repo,
                [
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "-m",
                    "first",
                ],
            );
            let first_sha = PinnedSha::new(git_text(&repo, ["rev-parse", "HEAD"])).unwrap();
            Self {
                _temp: temp,
                repo,
                owned,
                first_sha,
            }
        }

        fn manager(&self) -> WorktreeManager {
            WorktreeManager::new(&self.repo, &self.owned).unwrap()
        }
    }

    fn git<const N: usize>(cwd: &Path, args: [&str; N]) {
        let status = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn git_text<const N: usize>(cwd: &Path, args: [&str; N]) -> String {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn ids() -> (RunId, TaskId) {
        (RunId::generate(), TaskId::generate())
    }

    #[test]
    fn creates_lists_and_removes_a_detached_pinned_worktree() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        let (run_id, task_id) = ids();
        let added = manager
            .add(run_id.clone(), task_id.clone(), fixture.first_sha.clone())
            .unwrap();

        assert!(added.path.starts_with(manager.owned_root()));
        assert_eq!(added.base_sha, fixture.first_sha);
        assert_eq!(added.head_sha, fixture.first_sha);
        assert_eq!(added.status, WorktreeStatus::Active);
        assert!(!added.dirty);
        assert_eq!(
            fs::read_to_string(added.path.join("tracked.txt")).unwrap(),
            "first\n"
        );
        assert_eq!(manager.list().unwrap(), vec![added.clone()]);

        assert_eq!(manager.remove(&run_id, &task_id).unwrap(), added);
        assert!(manager.list().unwrap().is_empty());
    }

    #[test]
    fn uses_exact_base_commit_instead_of_current_head() {
        let fixture = Fixture::new();
        fs::write(fixture.repo.join("tracked.txt"), "second\n").unwrap();
        git(&fixture.repo, ["add", "tracked.txt"]);
        git(
            &fixture.repo,
            [
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "second",
            ],
        );

        let manager = fixture.manager();
        let (run_id, task_id) = ids();
        let added = manager
            .add(run_id, task_id, fixture.first_sha.clone())
            .unwrap();
        assert_eq!(added.head_sha, fixture.first_sha);
        assert_eq!(
            fs::read_to_string(added.path.join("tracked.txt")).unwrap(),
            "first\n"
        );
    }

    #[test]
    fn dirty_failed_and_cancelled_worktrees_survive_cleanup() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        let (failed_run, failed_task) = ids();
        let failed = manager
            .add(
                failed_run.clone(),
                failed_task.clone(),
                fixture.first_sha.clone(),
            )
            .unwrap();
        manager
            .set_status(&failed_run, &failed_task, WorktreeStatus::Failed)
            .unwrap();
        fs::write(failed.path.join("failure.log"), "preserve me\n").unwrap();

        let (cancelled_run, cancelled_task) = ids();
        let cancelled = manager
            .add(
                cancelled_run.clone(),
                cancelled_task.clone(),
                fixture.first_sha.clone(),
            )
            .unwrap();
        manager
            .set_status(&cancelled_run, &cancelled_task, WorktreeStatus::Cancelled)
            .unwrap();
        fs::write(cancelled.path.join("tracked.txt"), "changed\n").unwrap();

        let report = manager.cleanup().unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.preserved.len(), 2);
        assert!(
            report
                .preserved
                .iter()
                .all(|entry| entry.reason == PreservationReason::Dirty)
        );
        assert!(failed.path.exists());
        assert!(cancelled.path.exists());
        assert!(matches!(
            manager.remove(&failed_run, &failed_task),
            Err(WorktreeError::DirtyWorktree(_))
        ));
    }

    #[test]
    fn cleanup_removes_only_clean_terminal_registrations() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        let (active_run, active_task) = ids();
        let active = manager
            .add(
                active_run.clone(),
                active_task.clone(),
                fixture.first_sha.clone(),
            )
            .unwrap();
        let (complete_run, complete_task) = ids();
        let complete = manager
            .add(
                complete_run.clone(),
                complete_task.clone(),
                fixture.first_sha.clone(),
            )
            .unwrap();
        manager
            .set_status(&complete_run, &complete_task, WorktreeStatus::Complete)
            .unwrap();

        let report = manager.cleanup().unwrap();
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].run_id, complete_run);
        assert_eq!(report.preserved.len(), 1);
        assert_eq!(report.preserved[0].reason, PreservationReason::Active);
        assert!(active.path.exists());
        assert!(!complete.path.exists());
    }

    #[test]
    fn cleanup_ignores_unregistered_worktrees_even_under_owned_root() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        let foreign = manager.owned_root().join("foreign");
        let status = Command::new("git")
            .current_dir(&fixture.repo)
            .args(["worktree", "add", "--detach", "--"])
            .arg(&foreign)
            .arg(fixture.first_sha.as_str())
            .status()
            .unwrap();
        assert!(status.success());

        let report = manager.cleanup().unwrap();
        assert_eq!(report, CleanupReport::default());
        assert!(foreign.exists());
    }

    #[test]
    fn rejects_non_root_repositories_and_owned_roots_inside_repo() {
        let fixture = Fixture::new();
        let nested = fixture.repo.join("nested");
        fs::create_dir(&nested).unwrap();
        assert!(matches!(
            WorktreeManager::new(&nested, &fixture.owned),
            Err(WorktreeError::NotRepositoryRoot(_))
        ));
        assert!(matches!(
            WorktreeManager::new(&fixture.repo, fixture.repo.join("worktrees")),
            Err(WorktreeError::OwnedRootInsideRepository)
        ));
    }

    #[test]
    fn duplicate_keys_and_unknown_base_commits_are_rejected() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        let (run_id, task_id) = ids();
        manager
            .add(run_id.clone(), task_id.clone(), fixture.first_sha.clone())
            .unwrap();
        assert!(matches!(
            manager.add(run_id, task_id, fixture.first_sha),
            Err(WorktreeError::AlreadyManaged(_))
        ));

        let (run_id, task_id) = ids();
        let missing = PinnedSha::new("0000000000000000000000000000000000000000").unwrap();
        assert!(matches!(
            manager.add(run_id, task_id, missing),
            Err(WorktreeError::Git {
                operation: "verify base commit",
                ..
            })
        ));
    }

    #[test]
    fn rejects_repository_configured_executable_filters() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        git(
            &fixture.repo,
            ["config", "filter.untrusted.smudge", "untrusted-command"],
        );
        let (run_id, task_id) = ids();
        assert!(matches!(
            manager.add(run_id, task_id, fixture.first_sha),
            Err(WorktreeError::ExecutableGitFilters(_))
        ));
        assert_eq!(manager.list().unwrap(), Vec::<ManagedWorktree>::new());
    }

    #[test]
    fn strict_registry_rejects_unknown_fields_without_removing_worktree() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        let (run_id, task_id) = ids();
        let added = manager
            .add(run_id.clone(), task_id.clone(), fixture.first_sha.clone())
            .unwrap();
        let key = WorktreeKey { run_id, task_id };
        let registration_path = manager.registration_path(&key);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&registration_path).unwrap()).unwrap();
        value["unexpected"] = serde_json::json!(true);
        fs::write(&registration_path, serde_json::to_vec(&value).unwrap()).unwrap();

        assert!(matches!(
            manager.list(),
            Err(WorktreeError::InvalidRegistry(_))
        ));
        assert!(added.path.exists());
    }
}
