//! Strict Linux shell wrapper used by OpenCode's built-in ShellTool.
//!
//! Everything that actually enters the sandbox is Linux-only. Other targets
//! keep the shared types and the entry points that report the feature as
//! unavailable, which leaves the rest of the module legitimately unreferenced
//! rather than tempting a per-item `cfg` that drifts out of sync.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub const REGISTRY_REL: &str = ".config/opencode/.rtrt-sandbox-state.json";
const STATE_OWNER: &str = "rtrt-opencode-linux-sandbox";
const STATE_VERSION: u64 = 2;
const MAX_PROJECTS: usize = 128;
const MAX_REGISTRY_BYTES: u64 = 256 * 1024;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(target_os = "linux")]
const PRIVATE_SCRATCH: &str = "/rtrt-tmp";
#[cfg(target_os = "linux")]
const PRIVATE_CARGO_HOME: &str = "/rtrt-tmp/cargo-home";
#[cfg(target_os = "linux")]
const PRIVATE_RUSTUP_HOME: &str = "/rtrt-tmp/rustup-home";

#[derive(Debug, Clone)]
pub struct ProjectBoundary {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub git_writable: Vec<PathBuf>,
}

pub fn dispatch_from_env() -> Result<Option<i32>> {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_none_or(|arg| arg != "-c") {
        return Ok(None);
    }
    if args.len() != 3 {
        bail!("sandbox shell wrapper expects exactly: rtrt -c <command>");
    }
    let command = args[2]
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("sandbox command is not valid UTF-8"))?;
    if let Some(argv) = direct_claude_argv(command)? {
        return run_direct_claude(&argv).map(Some);
    }
    validate_command(command)?;
    run(command).map(Some)
}

/// Parse only the quoting needed by RTRT's generated Claude template. Shell
/// operators are rejected before any process exists; quoted prompt text stays
/// one argv element and is never evaluated.
fn direct_claude_argv(input: &str) -> Result<Option<Vec<String>>> {
    let trimmed = input.trim_start();
    if !trimmed
        .strip_prefix("claude")
        .is_some_and(|tail| tail.chars().next().is_some_and(char::is_whitespace))
    {
        return Ok(None);
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = input.chars().peekable();
    let mut started = false;
    while let Some(ch) = chars.next() {
        match ch {
            c if c.is_ascii_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => bail!("unterminated single quote in direct Claude command"),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some(c @ ('$' | '`' | '\\')) => bail!(
                            "direct Claude command rejects expansion-capable double quote: {c}"
                        ),
                        Some(c) => word.push(c),
                        None => bail!("unterminated double quote in direct Claude command"),
                    }
                }
            }
            c @ (';' | '|' | '&' | '(' | ')' | '<' | '>' | '`' | '\n' | '\r' | '\\') => {
                if words.first().is_some_and(|first| first == "claude") || word == "claude" {
                    bail!("direct Claude command rejects shell operator or expansion: {c}");
                }
                return Ok(None);
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        words.push(word);
    }
    if words.first().is_none_or(|word| word != "claude") {
        return Ok(None);
    }
    rtrt_core::detect::StrictClaudeInvocation::parse(&words)
        .map_err(|error| anyhow::anyhow!(error))?;
    Ok(Some(words))
}

fn run_direct_claude(argv: &[String]) -> Result<i32> {
    let boundary = discover_project()?;
    let registry = load_registry()?;
    project_tool_paths(&registry, &boundary)?;
    // Trust boundary: Claude itself receives normal provider authentication and
    // supplies the supported built-in sandbox. Its generated sandbox settings
    // deny credential files/env to child tools; wrapping or clearing Claude's
    // own environment here would break provider authentication.
    let mut command = rtrt_core::detect::strict_claude_command(argv, &boundary.root, None)
        .map_err(|error| anyhow::anyhow!(error))?;
    let status = command.status().context("spawn strict direct Claude CLI")?;
    Ok(status.code().unwrap_or(1))
}

fn validate_command(command: &str) -> Result<()> {
    if command.contains('\0') {
        bail!("sandbox command contains NUL");
    }
    if command.trim().is_empty() {
        bail!("sandbox command is empty");
    }
    if command
        .split(|character: char| character.is_ascii_whitespace() || ";|&()<>`".contains(character))
        .map(dequote_shell_word)
        .any(|word| Path::new(&word).file_name() == Some(OsStr::new("claude")))
        || command_segments(command).any(expansion_in_command_word)
    {
        bail!(
            "direct Claude CLI command refused: only canonical, unwrapped strict Claude invocation is accepted"
        );
    }
    Ok(())
}

fn dequote_shell_word(word: &str) -> String {
    word.chars()
        .filter(|character| !matches!(character, '\'' | '"' | '\\'))
        .collect()
}

fn command_segments(command: &str) -> impl Iterator<Item = &str> {
    command.split([';', '|', '&', '\n', '\r', '(', ')'])
}

/// Shell expansion in command position can synthesize an executable name
/// without a literal `claude` token. This deliberately does not evaluate the
/// shell; uncertain executable constructions fail closed.
fn expansion_in_command_word(segment: &str) -> bool {
    let mut words = segment.trim_start().split_ascii_whitespace();
    let mut word = words.next();
    if word.is_some_and(|value| matches!(value, "env" | "command" | "exec" | "nohup")) {
        word = words.find(|value| !value.starts_with('-') && !value.contains('='));
    }
    word.is_some_and(|value| value.contains('$') || value.contains('`'))
}

pub fn preflight() -> Result<PathBuf> {
    #[cfg(not(target_os = "linux"))]
    bail!("strict OpenCode sandbox is currently available only on Linux");
    #[cfg(target_os = "linux")]
    locate_backend_in(&[Path::new("/usr/bin/bwrap"), Path::new("/bin/bwrap")])
}

#[cfg(target_os = "linux")]
pub fn preflight_usable() -> Result<PathBuf> {
    let backend = preflight()?;
    let mut probe = Command::new(&backend);
    probe.args([
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--disable-userns",
        "--assert-userns-disabled",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-net",
        "--cap-drop",
        "ALL",
        "--ro-bind",
        "/usr",
        "/usr",
    ]);
    for (link, target) in [
        ("/bin", "usr/bin"),
        ("/lib", "usr/lib"),
        ("/lib64", "usr/lib64"),
    ] {
        if Path::new(link).is_symlink() {
            probe.arg("--symlink").arg(target).arg(link);
        } else if Path::new(link).exists() {
            probe.arg("--ro-bind").arg(link).arg(link);
        }
    }
    let status = probe
        .args(["--proc", "/proc", "--dev", "/dev", "--", "/bin/true"])
        .status()
        .with_context(|| format!("execute sandbox backend preflight {}", backend.display()))?;
    if !status.success() {
        bail!(
            "strict OpenCode sandbox backend is installed but unusable ({} exited {status}); check unprivileged user-namespace policy",
            backend.display()
        );
    }
    Ok(backend)
}

#[cfg(not(target_os = "linux"))]
pub fn preflight_usable() -> Result<PathBuf> {
    preflight()
}

#[cfg(target_os = "linux")]
fn locate_backend_in(candidates: &[&Path]) -> Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    for candidate in candidates {
        let Ok(metadata) = std::fs::symlink_metadata(candidate) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        let Ok(canonical) = std::fs::canonicalize(candidate) else {
            continue;
        };
        let Ok(canonical_metadata) = std::fs::metadata(&canonical) else {
            continue;
        };
        let mode = canonical_metadata.permissions().mode();
        if canonical_metadata.uid() == 0
            && canonical_metadata.is_file()
            && mode & 0o111 != 0
            && mode & 0o022 == 0
        {
            return Ok(canonical);
        }
    }
    bail!(
        "strict OpenCode sandbox unavailable: install a root-owned, executable, non-group/world-writable bubblewrap at /usr/bin/bwrap or /bin/bwrap"
    )
}

pub fn discover_project() -> Result<ProjectBoundary> {
    discover_project_from(&std::env::current_dir().context("resolve actual current directory")?)
}

/// Discover a checkout from an explicit launcher selection rather than the
/// caller's cwd. The returned root is the distinct writable worktree boundary;
/// linked worktrees validate their shared common Git controls separately.
pub fn discover_project_from(selected: &Path) -> Result<ProjectBoundary> {
    let cwd = std::fs::canonicalize(selected)
        .with_context(|| format!("canonicalize selected project {}", selected.display()))?;
    if !cwd.is_dir() {
        bail!("selected project is not a directory: {}", cwd.display());
    }
    if cwd == Path::new("/") {
        bail!("sandbox refuses filesystem root as current directory");
    }
    for ancestor in cwd.ancestors() {
        let git = ancestor.join(".git");
        let Ok(metadata) = std::fs::symlink_metadata(&git) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            bail!("sandbox refuses symlinked Git metadata: {}", git.display());
        }
        let root = std::fs::canonicalize(ancestor).context("canonicalize Git worktree")?;
        if root == Path::new("/") || !cwd.starts_with(&root) {
            bail!("sandbox worktree boundary is invalid");
        }
        let git_writable = if metadata.is_dir() {
            Vec::new()
        } else if metadata.is_file() {
            linked_git_dirs(&root, &git)?
        } else {
            bail!("sandbox Git metadata is neither a directory nor a file");
        };
        return Ok(ProjectBoundary {
            root,
            cwd,
            git_writable,
        });
    }
    bail!("sandbox requires current directory inside a canonical Git worktree")
}

#[cfg(target_os = "linux")]
fn validate_config_file(path: &Path, expected_uid: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !path.is_absolute() {
        bail!("OpenCode config path is not absolute: {}", path.display());
    }
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect OpenCode config {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("OpenCode config is not a regular file: {}", path.display());
    }
    if metadata.uid() != expected_uid {
        bail!("OpenCode config is not owned by the invoking user");
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        bail!(
            "OpenCode config is group/world-writable: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        bail!("OpenCode config exceeds {MAX_CONFIG_BYTES} bytes");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn validate_machine_config(path: &Path) -> Result<()> {
    validate_config_file(path, current_linux_uid()?)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn validate_machine_config(_path: &Path) -> Result<()> {
    bail!("strict OpenCode sandbox is currently available only on Linux")
}

#[cfg(target_os = "linux")]
struct RegistryLock {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

#[cfg(target_os = "linux")]
impl Drop for RegistryLock {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        if std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink()
                && metadata.is_file()
                && metadata.dev() == self.dev
                && metadata.ino() == self.ino
        }) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(target_os = "linux")]
fn acquire_registry_lock(registry_path: &Path, expected_uid: u32) -> Result<RegistryLock> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let path = registry_path.with_file_name(".rtrt-sandbox-state.lock");
    let started = std::time::Instant::now();
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                writeln!(
                    file,
                    "pid={} created={:?}",
                    std::process::id(),
                    std::time::SystemTime::now()
                )?;
                file.sync_all()?;
                let metadata = file.metadata()?;
                return Ok(RegistryLock {
                    path,
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&path)
                    .with_context(|| format!("inspect sandbox registry lock {}", path.display()))?;
                if metadata.file_type().is_symlink()
                    || !metadata.is_file()
                    || metadata.uid() != expected_uid
                    || metadata.permissions().mode() & 0o077 != 0
                {
                    bail!(
                        "sandbox registry lock has unsafe type, owner, or mode: {}",
                        path.display()
                    );
                }
                if started.elapsed() >= LOCK_WAIT {
                    let stale = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.elapsed().ok())
                        .is_some_and(|age| age >= LOCK_STALE);
                    bail!(
                        "sandbox registry lock wait timed out{}: {}",
                        if stale {
                            " (stale lock retained for safe operator inspection)"
                        } else {
                            ""
                        },
                        path.display()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create sandbox registry lock {}", path.display()));
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn with_registry_lock<T>(
    registry_path: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let parent = registry_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("sandbox registry has no parent"))?;
    if let Ok(metadata) = std::fs::symlink_metadata(parent)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        bail!(
            "sandbox registry directory is not a real directory: {}",
            parent.display()
        );
    }
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let uid = current_linux_uid()?;
    let metadata = std::fs::symlink_metadata(parent)?;
    if (metadata.uid() != uid && metadata.uid() != 0) || metadata.permissions().mode() & 0o022 != 0
    {
        bail!(
            "sandbox registry directory has unsafe owner or mode: {}",
            parent.display()
        );
    }
    let _lock = acquire_registry_lock(registry_path, uid)?;
    operation()
}

fn one_line(path: &Path, prefix: &str) -> Result<String> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut lines = raw.lines();
    let line = lines.next().unwrap_or_default();
    if lines.next().is_some() || !line.starts_with(prefix) || line.contains('\0') {
        bail!("invalid linked-worktree metadata: {}", path.display());
    }
    let value = line[prefix.len()..].trim();
    if value.is_empty() {
        bail!("empty linked-worktree metadata: {}", path.display());
    }
    Ok(value.to_string())
}

fn linked_git_dirs(root: &Path, git_file: &Path) -> Result<Vec<PathBuf>> {
    let git_dir = std::fs::canonicalize(root.join(one_line(git_file, "gitdir:")?))
        .context("canonicalize worktree Git admin directory")?;
    if !git_dir.is_dir() {
        bail!("linked-worktree Git admin path is not a directory");
    }
    let common = std::fs::canonicalize(git_dir.join(one_line(&git_dir.join("commondir"), "")?))
        .context("canonicalize common Git directory")?;
    if common.file_name() != Some(OsStr::new(".git")) || !common.is_dir() {
        bail!("linked-worktree common directory is not a validated .git directory");
    }
    let worktrees = std::fs::canonicalize(common.join("worktrees"))
        .context("canonicalize Git worktrees directory")?;
    let relative = git_dir
        .strip_prefix(&worktrees)
        .context("Git admin directory escapes common worktrees directory")?;
    if relative.components().count() != 1 {
        bail!("Git admin directory is not a direct worktrees child");
    }
    let backlink = std::fs::canonicalize(git_dir.join(one_line(&git_dir.join("gitdir"), "")?))
        .context("canonicalize Git worktree backlink")?;
    if backlink != std::fs::canonicalize(git_file).context("canonicalize worktree .git file")? {
        bail!("linked-worktree Git backlink mismatch");
    }
    Ok(vec![common, git_dir])
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn with_registry_lock<T>(
    _registry_path: &Path,
    _operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    bail!("strict OpenCode sandbox registry is currently available only on Linux")
}

pub fn registry_path() -> Result<PathBuf> {
    #[cfg(not(target_os = "linux"))]
    bail!("strict OpenCode sandbox registry is currently available only on Linux");
    #[cfg(target_os = "linux")]
    Ok(linux_home()?.join(REGISTRY_REL))
}

/// Validate the exact executable OpenCode will invoke as its shell wrapper.
/// The complete path is trusted only when neither the file nor any directory
/// used to reach it can be replaced by another local user.
#[cfg(target_os = "linux")]
pub fn validate_opencode_shell_executable(
    boundary: &ProjectBoundary,
    executable: &Path,
) -> Result<()> {
    validate_opencode_shell_executable_for_uid(
        Some(&boundary.root),
        executable,
        current_linux_uid()?,
        false,
    )
}

/// Validate machine-global trust without deriving or authorizing a project.
#[cfg(target_os = "linux")]
pub fn validate_machine_executable(executable: &Path) -> Result<()> {
    validate_opencode_shell_executable_for_uid(None, executable, current_linux_uid()?, true)
}

#[cfg(not(target_os = "linux"))]
pub fn validate_machine_executable(_executable: &Path) -> Result<()> {
    bail!("strict OpenCode sandbox is currently available only on Linux")
}

#[cfg(target_os = "linux")]
fn validate_opencode_shell_executable_for_uid(
    project_root: Option<&Path>,
    executable: &Path,
    uid: u32,
    reject_git_checkout: bool,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !executable.is_absolute() {
        bail!(
            "OpenCode sandbox shell executable must be an absolute canonical path: {}",
            executable.display()
        );
    }
    if project_root.is_some_and(|root| executable.starts_with(root)) {
        bail!(
            "OpenCode sandbox shell executable must be outside the writable authorized project root: {}",
            executable.display()
        );
    }
    let metadata = std::fs::symlink_metadata(executable)
        .with_context(|| format!("inspect sandbox shell executable {}", executable.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.permissions().mode() & 0o022 != 0
    {
        bail!(
            "OpenCode sandbox shell executable must be a real executable file not writable by group or others: {}",
            executable.display()
        );
    }
    if metadata.uid() != uid && metadata.uid() != 0 {
        bail!(
            "OpenCode sandbox shell executable must be owned by the invoking user or root: {}",
            executable.display()
        );
    }

    for ancestor in executable
        .parent()
        .ok_or_else(|| anyhow::anyhow!("sandbox shell executable has no parent"))?
        .ancestors()
    {
        if reject_git_checkout && std::fs::symlink_metadata(ancestor.join(".git")).is_ok() {
            bail!(
                "OpenCode sandbox shell executable must not be installed inside a Git checkout: {}",
                executable.display()
            );
        }
        let metadata = std::fs::symlink_metadata(ancestor).with_context(|| {
            format!(
                "inspect sandbox shell executable ancestor {}",
                ancestor.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "OpenCode sandbox shell executable ancestor must be a real directory, not a symlink: {}",
                ancestor.display()
            );
        }
        if metadata.uid() != uid && metadata.uid() != 0 {
            bail!(
                "OpenCode sandbox shell executable ancestor must be owned by the invoking user or root: {}",
                ancestor.display()
            );
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            bail!(
                "OpenCode sandbox shell executable ancestor must not be group/world-writable: {}",
                ancestor.display()
            );
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn validate_opencode_shell_executable(
    _boundary: &ProjectBoundary,
    _executable: &Path,
) -> Result<()> {
    bail!("strict OpenCode sandbox is currently available only on Linux")
}

/// Validate an executable launched directly by `rtrt opencode`. Unlike the
/// shell-wrapper policy, this is independent of bubblewrap and works on Unix
/// platforms where ownership and mode checks are available.
#[cfg(unix)]
pub fn validate_direct_launch_executable(
    boundary: &ProjectBoundary,
    executable: &Path,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    let trusted_uid = current_linux_uid()?;
    #[cfg(not(target_os = "linux"))]
    let trusted_uid = {
        use std::os::unix::fs::MetadataExt;

        let home = crate::setup::dirs_home()?;
        let home = std::fs::canonicalize(home).context("canonicalize invoking user's home")?;
        std::fs::metadata(&home)
            .context("inspect invoking user's home")?
            .uid()
    };
    validate_direct_launch_executable_for_uid(boundary, executable, trusted_uid)
}

#[cfg(unix)]
fn validate_direct_launch_executable_for_uid(
    boundary: &ProjectBoundary,
    executable: &Path,
    uid: u32,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !executable.is_absolute()
        || executable
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!(
            "direct-launch executable must be an absolute normalized path: {}",
            executable.display()
        );
    }
    if executable.starts_with(&boundary.root) {
        bail!(
            "direct-launch executable must be outside the writable checkout: {}",
            executable.display()
        );
    }
    let metadata = std::fs::symlink_metadata(executable)
        .with_context(|| format!("inspect direct-launch executable {}", executable.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.permissions().mode() & 0o022 != 0
    {
        bail!(
            "direct-launch executable must be a regular executable file, not a symlink, and not group/world-writable: {}",
            executable.display()
        );
    }
    if metadata.uid() != uid && metadata.uid() != 0 {
        bail!(
            "direct-launch executable must be owned by the invoking user/home owner or root: {}",
            executable.display()
        );
    }
    for ancestor in executable
        .parent()
        .ok_or_else(|| anyhow::anyhow!("direct-launch executable has no parent"))?
        .ancestors()
    {
        let metadata = std::fs::symlink_metadata(ancestor).with_context(|| {
            format!(
                "inspect direct-launch executable ancestor {}",
                ancestor.display()
            )
        })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.permissions().mode() & 0o022 != 0
        {
            bail!(
                "direct-launch executable ancestor must be a real directory not writable by group or others: {}",
                ancestor.display()
            );
        }
        if metadata.uid() != uid && metadata.uid() != 0 {
            bail!(
                "direct-launch executable ancestor must be owned by the invoking user/home owner or root: {}",
                ancestor.display()
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn validate_direct_launch_executable(
    _boundary: &ProjectBoundary,
    _executable: &Path,
) -> Result<()> {
    bail!(
        "trusted direct-launch executable security validation is unsupported on Windows; refusing OpenCode launch"
    )
}

pub fn new_registry(
    executable: &Path,
    backend: &Path,
    config: &Path,
    prior_shell: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "owner": STATE_OWNER,
        "version": STATE_VERSION,
        "executable": executable,
        "backend": backend,
        "config_path": config,
        "owned_shell": executable,
        "prior_shell": prior_shell,
        "projects": {},
    }))
}

pub fn authorize_project(
    registry: &mut serde_json::Value,
    boundary: &ProjectBoundary,
) -> Result<()> {
    authorize_project_with_tool_paths(registry, boundary, validated_tool_paths()?)
}

fn authorize_project_with_tool_paths(
    registry: &mut serde_json::Value,
    boundary: &ProjectBoundary,
    tool_paths: Vec<PathBuf>,
) -> Result<()> {
    let projects = registry
        .get_mut("projects")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("sandbox registry projects is not an object"))?;
    let key = boundary
        .root
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("project path is not valid UTF-8"))?;
    if !projects.contains_key(key) && projects.len() >= MAX_PROJECTS {
        bail!("sandbox registry project limit ({MAX_PROJECTS}) reached");
    }
    projects.insert(
        key.to_string(),
        serde_json::json!({"root": boundary.root, "tool_paths": tool_paths}),
    );
    Ok(())
}

pub fn deauthorize_project(
    registry: &mut serde_json::Value,
    boundary: &ProjectBoundary,
) -> Result<bool> {
    let projects = registry
        .get_mut("projects")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("sandbox registry projects is not an object"))?;
    let key = boundary
        .root
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("project path is not valid UTF-8"))?;
    if projects.remove(key).is_none() {
        bail!(
            "project is not authorized for strict sandboxing; run `rtrt setup --agent opencode --sandbox --apply` here"
        );
    }
    Ok(projects.is_empty())
}

pub(crate) fn write_registry_at(path: &Path, registry: &serde_json::Value) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("sandbox registry has no parent"))?;
    if let Ok(metadata) = std::fs::symlink_metadata(dir)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        bail!(
            "sandbox registry directory is not a real directory: {}",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(registry)?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        bail!("sandbox ownership registry exceeds {MAX_REGISTRY_BYTES} bytes");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let temporary = dir.join(format!(
            ".rtrt-sandbox-state.tmp-{}-{nonce}",
            std::process::id()
        ));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .with_context(|| format!("create {}", temporary.display()))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)
                .with_context(|| format!("replace {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
    #[cfg(not(unix))]
    rtrt_core::write_private_file_atomic(path, &bytes)
        .context("write private sandbox ownership registry")
}

#[cfg(target_os = "linux")]
pub fn load_registry() -> Result<serde_json::Value> {
    let path = registry_path()?;
    load_registry_at(
        &path,
        current_linux_uid()?,
        &std::fs::canonicalize(std::env::current_exe()?)?,
    )
}

#[cfg(target_os = "linux")]
fn validate_launch_state(
    registry: &serde_json::Value,
    boundary: &ProjectBoundary,
    executable: &Path,
    backend: &Path,
    expected_config: &Path,
    tool_paths: &[PathBuf],
) -> Result<bool> {
    if registry.get("backend").and_then(serde_json::Value::as_str) != backend.to_str()
        || registry
            .get("config_path")
            .and_then(serde_json::Value::as_str)
            != expected_config.to_str()
        || registry
            .get("owned_shell")
            .and_then(serde_json::Value::as_str)
            != executable.to_str()
    {
        bail!("sandbox registry config/executable/backend does not match one-time setup");
    }
    validate_config_file(expected_config, current_linux_uid()?)?;
    crate::setup::validate_opencode_sandbox_shell(expected_config, executable)?;
    match project_tool_paths(registry, boundary) {
        Ok(recorded) => {
            if recorded != tool_paths {
                bail!("sandbox toolchain paths no longer match setup-managed policy");
            }
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// Validate one-time machine setup and atomically add only the selected
/// checkout boundary when absent. No setup-owned surface other than registry
/// membership is ever changed here.
#[cfg(target_os = "linux")]
pub fn authorize_project_for_launch(boundary: &ProjectBoundary) -> Result<bool> {
    let executable = std::fs::canonicalize(std::env::current_exe()?)
        .context("canonicalize installed rtrt executable")?;
    validate_opencode_shell_executable(boundary, &executable)?;
    let backend = preflight_usable()?;
    let registry_path = registry_path()?;
    let config = crate::setup::resolve_opencode_config_path()?;
    let tool_paths = validated_tool_paths()?;
    authorize_project_at(
        &registry_path,
        boundary,
        &executable,
        &backend,
        &config,
        tool_paths,
    )
}

#[cfg(target_os = "linux")]
fn authorize_project_at(
    registry_path: &Path,
    boundary: &ProjectBoundary,
    executable: &Path,
    backend: &Path,
    config: &Path,
    tool_paths: Vec<PathBuf>,
) -> Result<bool> {
    let uid = current_linux_uid()?;
    if registry_path.exists() {
        let initial = load_registry_at(registry_path, uid, executable)?;
        if validate_launch_state(&initial, boundary, executable, backend, config, &tool_paths)? {
            return Ok(false);
        }
    }

    let _lock = acquire_registry_lock(registry_path, uid)?;
    // Compare/reload under lock: another launcher may have registered a
    // disjoint checkout after our optimistic read.
    let mut current = load_registry_at(registry_path, uid, executable)?;
    if validate_launch_state(&current, boundary, executable, backend, config, &tool_paths)? {
        return Ok(false);
    }
    authorize_project_with_tool_paths(&mut current, boundary, tool_paths)?;
    write_registry_at(registry_path, &current)?;
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
pub fn authorize_project_for_launch(_boundary: &ProjectBoundary) -> Result<bool> {
    bail!("strict OpenCode sandbox is currently available only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub fn load_registry() -> Result<serde_json::Value> {
    bail!("strict OpenCode sandbox registry is currently available only on Linux")
}

#[cfg(target_os = "linux")]
fn load_registry_at(
    path: &Path,
    expected_uid: u32,
    executable: &Path,
) -> Result<serde_json::Value> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "sandbox ownership registry missing: {}; run `rtrt setup --agent opencode --sandbox --apply` in this worktree",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "sandbox ownership registry is not a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_REGISTRY_BYTES {
        bail!("sandbox ownership registry exceeds {MAX_REGISTRY_BYTES} bytes");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!(
            "sandbox ownership registry is not private (expected mode 0600): {}",
            path.display()
        );
    }
    if metadata.uid() != expected_uid {
        bail!("sandbox ownership registry is not owned by the invoking user");
    }
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    if state.get("owner").and_then(|v| v.as_str()) != Some(STATE_OWNER)
        || state.get("version").and_then(|v| v.as_u64()) != Some(STATE_VERSION)
        || state.get("executable").and_then(|v| v.as_str()) != executable.to_str()
        || state.get("backend").and_then(|v| v.as_str()).is_none()
        || state.get("config_path").and_then(|v| v.as_str()).is_none()
        || state.get("owned_shell").and_then(|v| v.as_str()) != executable.to_str()
        || state.get("prior_shell").is_none()
        || state.get("projects").and_then(|v| v.as_object()).is_none()
    {
        bail!("sandbox ownership registry does not match exact executable policy");
    }
    if state["projects"]
        .as_object()
        .is_some_and(|p| p.len() > MAX_PROJECTS)
    {
        bail!("sandbox ownership registry exceeds bounded project limit");
    }
    for (root, entry) in state["projects"].as_object().expect("validated object") {
        if entry.get("root").and_then(|value| value.as_str()) != Some(root)
            || !Path::new(root).is_absolute()
        {
            bail!("sandbox ownership registry contains invalid project root");
        }
        let tools = entry
            .get("tool_paths")
            .and_then(|value| value.as_array())
            .ok_or_else(|| anyhow::anyhow!("sandbox registry project has invalid tool paths"))?;
        if tools.len() > 8 || tools.iter().any(|value| value.as_str().is_none()) {
            bail!("sandbox registry project tool paths exceed bounded policy");
        }
        if executable.starts_with(root) {
            bail!("sandbox executable is inside an authorized project root");
        }
    }
    validate_opencode_shell_executable_for_uid(None, executable, expected_uid, false)?;
    Ok(state)
}

#[cfg(target_os = "linux")]
pub(crate) fn load_registry_from(path: &Path, executable: &Path) -> Result<serde_json::Value> {
    load_registry_at(path, current_linux_uid()?, executable)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn load_registry_from(_path: &Path, _executable: &Path) -> Result<serde_json::Value> {
    bail!("strict OpenCode sandbox registry is currently available only on Linux")
}

pub(crate) fn remove_registry_at(path: &Path) -> Result<()> {
    std::fs::remove_file(path).context("remove sandbox ownership registry")
}

pub fn project_tool_paths(
    registry: &serde_json::Value,
    boundary: &ProjectBoundary,
) -> Result<Vec<PathBuf>> {
    let key = boundary
        .root
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("project path is not valid UTF-8"))?;
    let entry = registry
        .get("projects")
        .and_then(|v| v.as_object())
        .and_then(|projects| projects.get(key))
        .ok_or_else(|| anyhow::anyhow!(
            "project is not authorized for strict sandboxing; run `rtrt setup --agent opencode --sandbox --apply` here"
        ))?;
    if entry.get("root").and_then(|v| v.as_str()) != boundary.root.to_str() {
        bail!("sandbox project registry entry has mismatched canonical root");
    }
    entry
        .get("tool_paths")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("sandbox project registry entry has no tool paths"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("sandbox registry contains invalid tool path"))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn current_linux_uid() -> Result<u32> {
    let status = std::fs::read_to_string("/proc/self/status").context("read process identity")?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|line| line.split_ascii_whitespace().next())
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("cannot determine process uid"))?;
    Ok(uid)
}

#[cfg(target_os = "linux")]
fn linux_home() -> Result<PathBuf> {
    let uid = current_linux_uid()?;
    let passwd = std::fs::read_to_string("/etc/passwd").context("read local user database")?;
    let home = passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() >= 7 && fields[2].parse::<u32>().ok() == Some(uid))
            .then(|| PathBuf::from(fields[5]))
    });
    let home = home.ok_or_else(|| anyhow::anyhow!("cannot determine invoking user's home"))?;
    std::fs::canonicalize(home).context("canonicalize invoking user's home")
}

#[cfg(target_os = "linux")]
fn validated_tool_paths() -> Result<Vec<PathBuf>> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let uid = current_linux_uid()?;
    let home = linux_home()?;
    let mut paths = Vec::new();
    for candidate in [
        home.join(".cargo/bin"),
        home.join(".cargo/registry"),
        home.join(".cargo/git"),
        home.join(".local/bin"),
        home.join(".rustup/toolchains"),
        home.join(".rustup/settings.toml"),
    ] {
        let Ok(metadata) = std::fs::symlink_metadata(&candidate) else {
            continue;
        };
        if metadata.file_type().is_symlink()
            || metadata.uid() != uid
            || metadata.permissions().mode() & 0o022 != 0
            || (!metadata.is_dir() && !metadata.is_file())
        {
            continue;
        }
        paths.push(std::fs::canonicalize(candidate)?);
    }
    Ok(paths)
}

#[cfg(not(target_os = "linux"))]
fn validated_tool_paths() -> Result<Vec<PathBuf>> {
    Ok(Vec::new())
}

#[cfg(target_os = "linux")]
fn add_destination_parents(command: &mut Command, path: &Path) {
    let mut current = PathBuf::from("/");
    if let Some(parent) = path.parent() {
        for component in parent.components().skip(1) {
            current.push(component.as_os_str());
            command.arg("--dir").arg(&current);
        }
    }
}

#[cfg(target_os = "linux")]
fn bind_if_exists(command: &mut Command, source: &str) {
    let path = Path::new(source);
    if path.exists() {
        command.arg("--ro-bind").arg(path).arg(path);
    }
}

#[cfg(target_os = "linux")]
fn deterministic_path(tool_paths: &[PathBuf]) -> Result<String> {
    let home = linux_home()?;
    let mut parts = vec![
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
    ];
    for candidate in [home.join(".cargo/bin"), home.join(".local/bin")] {
        if tool_paths.contains(&candidate) {
            parts.push(candidate.to_string_lossy().into_owned());
        }
    }
    Ok(parts.join(":"))
}

#[cfg(target_os = "linux")]
fn validated_scratch_mount(boundary: &ProjectBoundary) -> Result<&'static Path> {
    let scratch = Path::new(PRIVATE_SCRATCH);
    let collides = std::iter::once(&boundary.root)
        .chain(boundary.git_writable.iter())
        .any(|destination| destination.starts_with(scratch) || scratch.starts_with(destination));
    if collides {
        bail!(
            "private sandbox scratch mount collides with an authorized project destination: {PRIVATE_SCRATCH}"
        );
    }
    Ok(scratch)
}

#[cfg(not(target_os = "linux"))]
fn run(_original: &str) -> Result<i32> {
    bail!("strict OpenCode sandbox is currently available only on Linux")
}

#[cfg(target_os = "linux")]
fn run(original: &str) -> Result<i32> {
    let backend = preflight()?;
    let boundary = discover_project()?;
    let registry = load_registry()?;
    if registry.get("backend").and_then(|v| v.as_str()) != backend.to_str() {
        bail!("sandbox backend no longer matches setup-managed ownership registry");
    }
    let recorded_tool_paths = project_tool_paths(&registry, &boundary)?;
    let tool_paths = validated_tool_paths()?;
    if tool_paths != recorded_tool_paths {
        bail!("sandbox toolchain paths no longer match setup-managed policy; rerun setup");
    }

    spawn_sandbox(original, &backend, &boundary, &tool_paths)
}

#[cfg(target_os = "linux")]
fn spawn_sandbox(
    original: &str,
    backend: &Path,
    boundary: &ProjectBoundary,
    tool_paths: &[PathBuf],
) -> Result<i32> {
    let scratch = validated_scratch_mount(boundary)?;
    let mut command = Command::new(backend);
    command.args([
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--disable-userns",
        "--assert-userns-disabled",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-net",
        "--cap-drop",
        "ALL",
        "--tmpfs",
        "/",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
    ]);
    command.arg("--tmpfs").arg(scratch);
    bind_if_exists(&mut command, "/usr");
    command.arg("--dir").arg("/etc");
    for runtime in [
        "/etc/ld.so.cache",
        "/etc/nsswitch.conf",
        "/etc/passwd",
        "/etc/group",
        "/etc/hosts",
        "/etc/localtime",
        "/etc/ssl/certs",
    ] {
        add_destination_parents(&mut command, Path::new(runtime));
        bind_if_exists(&mut command, runtime);
    }
    for (link, target) in [
        ("/bin", "usr/bin"),
        ("/sbin", "usr/sbin"),
        ("/lib", "usr/lib"),
        ("/lib64", "usr/lib64"),
    ] {
        if Path::new(link).is_symlink() {
            command.arg("--symlink").arg(target).arg(link);
        } else {
            bind_if_exists(&mut command, link);
        }
    }
    add_destination_parents(&mut command, &boundary.root);
    command
        .arg("--bind")
        .arg(&boundary.root)
        .arg(&boundary.root);
    for git in &boundary.git_writable {
        add_destination_parents(&mut command, git);
        command.arg("--bind").arg(git).arg(git);
    }
    // Cargo needs a writable CARGO_HOME for its package-cache lock even when
    // registry data is read-only and networking is disabled. Keep mutable
    // state private while exposing only validated cache/toolchain children.
    command
        .arg("--dir")
        .arg(PRIVATE_CARGO_HOME)
        .arg("--dir")
        .arg(PRIVATE_RUSTUP_HOME);
    let home = linux_home()?;
    for tool_path in tool_paths {
        let destination = tool_path
            .strip_prefix(home.join(".cargo"))
            .ok()
            .filter(|relative| relative.starts_with("registry") || relative.starts_with("git"))
            .map(|relative| Path::new(PRIVATE_CARGO_HOME).join(relative))
            .or_else(|| {
                tool_path
                    .strip_prefix(home.join(".rustup"))
                    .ok()
                    .map(|relative| Path::new(PRIVATE_RUSTUP_HOME).join(relative))
            })
            .unwrap_or_else(|| tool_path.clone());
        add_destination_parents(&mut command, &destination);
        command.arg("--ro-bind").arg(tool_path).arg(destination);
    }
    // Parent placeholders live on synthetic mounts. Freeze both the root and
    // /tmp after child binds are installed, so a checkout below /tmp cannot
    // create siblings. Only authorized child binds and private scratch remain
    // writable.
    command
        .arg("--remount-ro")
        .arg("/tmp")
        .arg("--remount-ro")
        .arg("/")
        .arg("--chdir")
        .arg(&boundary.cwd)
        .arg("--clearenv");
    for key in ["LANG", "TERM", "COLORTERM", "NO_COLOR"] {
        if let Some(value) = std::env::var_os(key) {
            command.arg("--setenv").arg(key).arg(value);
        }
    }
    for (key, value) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LC_") {
            command.arg("--setenv").arg(key).arg(value);
        }
    }
    command
        .args(["--setenv", "PATH"])
        .arg(deterministic_path(tool_paths)?)
        .args([
            "--setenv",
            "HOME",
            "/nonexistent",
            "--setenv",
            "TMPDIR",
            PRIVATE_SCRATCH,
        ]);
    command
        .args(["--setenv", "CARGO_HOME"])
        .arg(PRIVATE_CARGO_HOME)
        .args(["--setenv", "RUSTUP_HOME"])
        .arg(PRIVATE_RUSTUP_HOME)
        .args(["--setenv", "CARGO_NET_OFFLINE", "true"]);
    command
        .arg("--")
        .arg("/bin/bash")
        .args(["--noprofile", "--norc", "-c"])
        .arg(original);
    let status = command
        .status()
        .with_context(|| format!("spawn validated sandbox backend {}", backend.display()))?;
    Ok(status.code().unwrap_or(1))
}

/// Scratch root for fixture executables fed to the shell-executable validators.
///
/// Neither `TMPDIR` nor the repo works: the validator rejects every
/// group/world-writable ancestor (`/tmp` is 1777), and machine scope also
/// rejects any ancestor inside a Git checkout.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn private_scratch() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;

    let base = PathBuf::from(
        std::env::var_os("HOME").expect("HOME must be set to locate a private test scratch"),
    )
    .join(".cache/rtrt-sandbox-tests");
    std::fs::create_dir_all(&base).expect("create private test scratch base");
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
        .expect("restrict private test scratch base");
    tempfile::tempdir_in(&base).expect("create private test scratch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_validation_rejects_nul_and_noncanonical_claude() {
        assert!(validate_command("x\0y").is_err());
        assert!(validate_command("claude -p hi").is_err());
        assert!(validate_command("echo ok; claude -p hi").is_err());
        assert!(validate_command("echo claude-safe").is_ok());
    }

    #[test]
    fn command_validation_rejects_spliced_or_expanded_claude_executables() {
        for command in [
            "cla''ude -p x",
            "cl\"\"aude -p x",
            "'/usr/bin/cla'\"ude\" -p x",
            "/usr/bin/cl\\aude -p x",
            "$CLAUDE -p x",
            "${CLAUDE} -p x",
            "$(printf claude) -p x",
            "cl$(printf au)aude -p x",
            "echo ok; `printf claude` -p x",
            "env CLAUDE=claude \"$CLAUDE\" -p x",
        ] {
            assert!(validate_command(command).is_err(), "accepted {command}");
        }
        assert!(validate_command("echo \"$HOME\"").is_ok());
    }

    #[test]
    fn direct_claude_parser_accepts_only_generated_shape_and_one_prompt() {
        let valid = "claude -p --model sonnet --output-format json --permission-mode acceptEdits --permission-prompt-tool mcp__rtrt__permission_prompt 'fix x; do not expand $(id)'";
        let argv = direct_claude_argv(valid).unwrap().unwrap();
        assert_eq!(argv.last().unwrap(), "fix x; do not expand $(id)");
        assert!(direct_claude_argv("echo \"$HOME\"").unwrap().is_none());
        for invalid in [
            "env claude -p --model sonnet --output-format json --permission-mode acceptEdits --permission-prompt-tool mcp__rtrt__permission_prompt x",
            "claude -p --model haiku --output-format json --permission-mode plan --permission-prompt-tool mcp__rtrt__permission_prompt x",
            "claude -p --model opus --output-format json --permission-mode bypassPermissions --permission-prompt-tool mcp__rtrt__permission_prompt x",
            "claude -p --model opus --output-format json --permission-mode plan --allowed-tools Bash --permission-prompt-tool mcp__rtrt__permission_prompt x",
            "claude -p --model opus --output-format json --permission-mode plan --permission-prompt-tool mcp__rtrt__permission_prompt x | sh",
            "claude -p --model opus --output-format json --permission-mode plan --permission-prompt-tool mcp__rtrt__permission_prompt x > out",
            "claude -p --model opus --output-format json --permission-mode plan --permission-prompt-tool mcp__rtrt__permission_prompt $(id)",
            "claude -p --model opus --model sonnet --output-format json --permission-mode plan --permission-prompt-tool mcp__rtrt__permission_prompt x",
        ] {
            assert!(
                direct_claude_argv(invalid).is_err() || validate_command(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_backend_fails_closed() {
        assert!(locate_backend_in(&[Path::new("/definitely/missing/bwrap")]).is_err());
    }

    #[cfg(target_os = "linux")]
    fn test_boundary(root: &Path) -> ProjectBoundary {
        ProjectBoundary {
            root: root.to_path_buf(),
            cwd: root.to_path_buf(),
            git_writable: Vec::new(),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scratch_mount_is_disjoint_from_all_authorized_destinations() {
        let under_tmp = test_boundary(Path::new("/tmp/checkout/project"));
        assert_eq!(
            validated_scratch_mount(&under_tmp).unwrap(),
            Path::new(PRIVATE_SCRATCH)
        );

        let mut linked = test_boundary(Path::new("/home/user/worktree"));
        linked.git_writable = vec![PathBuf::from("/home/user/repository/.git")];
        assert_eq!(
            validated_scratch_mount(&linked).unwrap(),
            Path::new(PRIVATE_SCRATCH)
        );

        for destination in ["/rtrt-tmp", "/rtrt-tmp/checkout"] {
            assert!(validated_scratch_mount(&test_boundary(Path::new(destination))).is_err());
        }
        let mut colliding_git = test_boundary(Path::new("/home/user/worktree"));
        colliding_git.git_writable = vec![PathBuf::from("/rtrt-tmp/repository/.git")];
        assert!(validated_scratch_mount(&colliding_git).is_err());
    }

    // These fixtures need scratch space inside the workspace rather than the
    // system temp dir, whose ownership and symlinking the sandbox validator
    // legitimately rejects. `.rtrt/` is ignored by git, so a fresh checkout has
    // no `.rtrt/tmp` to find and the directory has to be created here.
    #[cfg(unix)]
    fn workspace_scratch_dir() -> PathBuf {
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let workspace = cwd
            .ancestors()
            .find(|ancestor| ancestor.join("Cargo.lock").is_file())
            .expect("workspace root containing Cargo.lock");
        let scratch = workspace.join(".rtrt").join("tmp");
        std::fs::create_dir_all(&scratch).unwrap();
        scratch
    }

    #[cfg(target_os = "linux")]
    fn trusted_shell_fixture() -> (tempfile::TempDir, ProjectBoundary, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir_in(workspace_scratch_dir()).unwrap();
        let project = temp.path().join("project");
        let bin = temp.path().join(".cargo/bin");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("rtrt");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let boundary = test_boundary(&project);
        (temp, boundary, bin, executable)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shell_executable_accepts_trusted_cargo_bin_shape_and_rejects_writable_parents() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, boundary, bin, executable) = trusted_shell_fixture();
        validate_opencode_shell_executable(&boundary, &executable).unwrap();

        for mode in [0o770, 0o707] {
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(validate_opencode_shell_executable(&boundary, &executable).is_err());
        }
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        validate_opencode_shell_executable(&boundary, &executable).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shell_executable_rejects_symlink_path_and_parent() {
        use std::os::unix::fs::symlink;

        let (temp, boundary, bin, executable) = trusted_shell_fixture();
        let executable_link = temp.path().join("rtrt-link");
        symlink(&executable, &executable_link).unwrap();
        assert!(validate_opencode_shell_executable(&boundary, &executable_link).is_err());

        let bin_link = temp.path().join("bin-link");
        symlink(&bin, &bin_link).unwrap();
        assert!(validate_opencode_shell_executable(&boundary, &bin_link.join("rtrt")).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shell_executable_rejects_foreign_ownership_policy_where_testable() {
        let (_temp, boundary, _bin, executable) = trusted_shell_fixture();
        let uid = current_linux_uid().unwrap();
        if uid != 0 {
            assert!(
                validate_opencode_shell_executable_for_uid(
                    Some(&boundary.root),
                    &executable,
                    uid + 1,
                    false,
                )
                .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn direct_launch_validator_accepts_trusted_file_and_rejects_checkout_or_symlink() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let fixture = tempfile::tempdir_in(workspace_scratch_dir()).unwrap();
        let project = fixture.path().join("project");
        let bin = fixture.path().join("bin");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&bin).unwrap();
        std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = bin.join("opencode");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let boundary = ProjectBoundary {
            root: project.clone(),
            cwd: project.clone(),
            git_writable: Vec::new(),
        };
        let uid = std::fs::metadata(fixture.path()).unwrap().uid();
        validate_direct_launch_executable_for_uid(&boundary, &executable, uid).unwrap();

        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(validate_direct_launch_executable_for_uid(&boundary, &executable, uid).is_err());
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(validate_direct_launch_executable_for_uid(&boundary, &executable, uid).is_err());
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            validate_direct_launch_executable_for_uid(
                &boundary,
                Path::new("relative-opencode"),
                uid
            )
            .is_err()
        );

        let link = bin.join("opencode-link");
        symlink(&executable, &link).unwrap();
        assert!(validate_direct_launch_executable_for_uid(&boundary, &link, uid).is_err());

        let checkout_executable = project.join("opencode");
        std::fs::write(&checkout_executable, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&checkout_executable, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert!(
            validate_direct_launch_executable_for_uid(&boundary, &checkout_executable, uid)
                .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn direct_launch_validator_fails_closed_with_precise_windows_error() {
        let boundary = ProjectBoundary {
            root: PathBuf::from(r"C:\project"),
            cwd: PathBuf::from(r"C:\project"),
            git_writable: Vec::new(),
        };
        let error = validate_direct_launch_executable(
            &boundary,
            Path::new(r"C:\Program Files\OpenCode\opencode.exe"),
        )
        .unwrap_err();
        assert!(error.to_string().contains(
            "trusted direct-launch executable security validation is unsupported on Windows"
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn registry_runtime_revalidation_rejects_post_setup_mode_change() {
        use std::os::unix::fs::PermissionsExt;

        let (temp, boundary, _bin, executable) = trusted_shell_fixture();
        let registry_path = temp.path().join("state.json");
        let mut registry = new_registry(
            &executable,
            Path::new("/usr/bin/bwrap"),
            Path::new("/fixed/opencode.json"),
            None,
        )
        .unwrap();
        authorize_project(&mut registry, &boundary).unwrap();
        write_registry_at(&registry_path, &registry).unwrap();
        load_registry_at(&registry_path, current_linux_uid().unwrap(), &executable).unwrap();

        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            load_registry_at(&registry_path, current_linux_uid().unwrap(), &executable).is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn global_registry_is_private_bounded_and_tracks_projects_independently() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = temp.path().join("state.json");
        let executable = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let backend = PathBuf::from("/fixed/bwrap");
        let mut registry = new_registry(
            &executable,
            &backend,
            Path::new("/fixed/opencode.json"),
            Some(serde_json::json!("/bin/zsh")),
        )
        .unwrap();
        let first = test_boundary(Path::new("/canonical/first"));
        let second = test_boundary(Path::new("/canonical/second"));
        let unknown = test_boundary(Path::new("/canonical/unknown"));
        authorize_project(&mut registry, &first).unwrap();
        authorize_project(&mut registry, &second).unwrap();
        assert!(project_tool_paths(&registry, &unknown).is_err());
        write_registry_at(&path, &registry).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let loaded = load_registry_at(&path, current_linux_uid().unwrap(), &executable).unwrap();
        assert_eq!(loaded["prior_shell"], "/bin/zsh");
        assert!(!deauthorize_project(&mut registry, &first).unwrap());
        assert!(project_tool_paths(&registry, &first).is_err());
        assert!(project_tool_paths(&registry, &second).is_ok());
        assert!(deauthorize_project(&mut registry, &second).unwrap());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_registry_at(&path, current_linux_uid().unwrap(), &executable).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_registry_at(&path, current_linux_uid().unwrap() + 1, &executable).is_err());
        let link = temp.path().join("state-link.json");
        symlink(&path, &link).unwrap();
        assert!(load_registry_at(&link, current_linux_uid().unwrap(), &executable).is_err());
        let mut tampered = loaded;
        tampered["owner"] = serde_json::json!("foreign");
        write_registry_at(&path, &tampered).unwrap();
        assert!(load_registry_at(&path, current_linux_uid().unwrap(), &executable).is_err());
    }

    #[cfg(target_os = "linux")]
    fn launch_authorization_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        PathBuf,
        ProjectBoundary,
    ) {
        use std::os::unix::fs::PermissionsExt;

        let (temp, boundary, _bin, executable) = trusted_shell_fixture();
        let config = temp.path().join("opencode.json");
        let registry = temp.path().join("state.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({"shell": executable})).unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_registry_at(
            &registry,
            &new_registry(&executable, Path::new("/fixed/bwrap"), &config, None).unwrap(),
        )
        .unwrap();
        (temp, registry, config, executable, boundary)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn launcher_authorization_adds_once_without_rewriting_checkout_or_registry() {
        use std::os::unix::fs::MetadataExt;

        let (_temp, registry, config, executable, boundary) = launch_authorization_fixture();
        std::fs::write(boundary.root.join("tracked"), "unchanged").unwrap();
        assert!(
            authorize_project_at(
                &registry,
                &boundary,
                &executable,
                Path::new("/fixed/bwrap"),
                &config,
                Vec::new(),
            )
            .unwrap()
        );
        let first = std::fs::read(&registry).unwrap();
        let inode = std::fs::metadata(&registry).unwrap().ino();
        assert!(
            project_tool_paths(
                &load_registry_at(&registry, current_linux_uid().unwrap(), &executable).unwrap(),
                &boundary
            )
            .is_ok()
        );

        assert!(
            !authorize_project_at(
                &registry,
                &boundary,
                &executable,
                Path::new("/fixed/bwrap"),
                &config,
                Vec::new(),
            )
            .unwrap()
        );
        assert_eq!(std::fs::read(&registry).unwrap(), first);
        assert_eq!(std::fs::metadata(&registry).unwrap().ino(), inode);
        assert_eq!(
            std::fs::read_to_string(boundary.root.join("tracked")).unwrap(),
            "unchanged"
        );
        assert_eq!(std::fs::read_dir(&boundary.root).unwrap().count(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn launcher_authorization_rejects_config_and_registry_tampering() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let (temp, registry, config, executable, boundary) = launch_authorization_fixture();
        let call = || {
            authorize_project_at(
                &registry,
                &boundary,
                &executable,
                Path::new("/fixed/bwrap"),
                &config,
                Vec::new(),
            )
        };

        std::fs::write(&config, r#"{"shell":"/tampered"}"#).unwrap();
        assert!(
            call()
                .unwrap_err()
                .to_string()
                .contains("does not exactly match")
        );
        std::fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({"shell": executable})).unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o622)).unwrap();
        assert!(call().is_err());
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(validate_config_file(&config, current_linux_uid().unwrap() + 1).is_err());

        let config_real = temp.path().join("config-real");
        std::fs::rename(&config, &config_real).unwrap();
        symlink(&config_real, &config).unwrap();
        assert!(call().is_err());
        std::fs::remove_file(&config).unwrap();
        std::fs::rename(&config_real, &config).unwrap();

        std::fs::rename(&config, &config_real).unwrap();
        assert!(call().is_err());
        std::fs::rename(&config_real, &config).unwrap();

        let registry_real = temp.path().join("registry-real");
        std::fs::rename(&registry, &registry_real).unwrap();
        symlink(&registry_real, &registry).unwrap();
        assert!(call().is_err());
        std::fs::remove_file(&registry).unwrap();
        std::fs::rename(&registry_real, &registry).unwrap();

        std::fs::rename(&registry, &registry_real).unwrap();
        assert!(call().is_err());
        std::fs::rename(&registry_real, &registry).unwrap();

        let lock = registry.with_file_name(".rtrt-sandbox-state.lock");
        symlink(&config, &lock).unwrap();
        assert!(call().is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn concurrent_disjoint_launcher_authorizations_are_retained() {
        let (_temp, registry, config, executable, first) = launch_authorization_fixture();
        let second_root = first.root.with_file_name("second-project");
        std::fs::create_dir(&second_root).unwrap();
        let second = test_boundary(&second_root);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let handles = [first.clone(), second.clone()].map(|boundary| {
            let registry = registry.clone();
            let config = config.clone();
            let executable = executable.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                authorize_project_at(
                    &registry,
                    &boundary,
                    &executable,
                    Path::new("/fixed/bwrap"),
                    &config,
                    Vec::new(),
                )
                .unwrap();
            })
        });
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }
        let state = load_registry_at(&registry, current_linux_uid().unwrap(), &executable).unwrap();
        assert!(project_tool_paths(&state, &first).is_ok());
        assert!(project_tool_paths(&state, &second).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn explicit_discovery_and_linked_worktrees_keep_distinct_checkout_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("same");
        let linked = temp.path().join("other/same");
        std::fs::create_dir_all(main.join(".git/worktrees/linked")).unwrap();
        std::fs::create_dir_all(&linked).unwrap();
        let dot_git = linked.join(".git");
        let admin = main.join(".git/worktrees/linked");
        std::fs::write(&dot_git, format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        std::fs::write(admin.join("gitdir"), format!("{}\n", dot_git.display())).unwrap();

        let selected = discover_project_from(&linked).unwrap();
        assert_eq!(selected.root, std::fs::canonicalize(&linked).unwrap());
        assert_eq!(selected.cwd, selected.root);
        assert_eq!(
            selected.git_writable[0],
            std::fs::canonicalize(main.join(".git")).unwrap()
        );
        assert_ne!(selected.root, std::fs::canonicalize(&main).unwrap());
    }

    #[cfg(target_os = "linux")]
    fn assert_sandbox_clause(
        label: &str,
        command: &str,
        backend: &Path,
        boundary: &ProjectBoundary,
        tool_paths: &[PathBuf],
    ) {
        let status = spawn_sandbox(command, backend, boundary, tool_paths)
            .unwrap_or_else(|error| panic!("sandbox invariant `{label}` could not run: {error:#}"));
        assert_eq!(
            status, 0,
            "sandbox invariant `{label}` failed (exit {status}); command: {command}"
        );
    }

    #[cfg(target_os = "linux")]
    fn shell_quote(value: &Path) -> String {
        format!("'{}'", value.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn approved_bwrap_runtime_scrubs_and_isolates() {
        let Ok(backend) = preflight_usable() else {
            // Host integration coverage is conditional on the operator-owned
            // backend; unit-only CI must not weaken production fail-closed.
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir(project.join("src")).unwrap();
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname='sandbox_fixture'\nversion='0.1.0'\nedition='2024'\n\n[dev-dependencies]\ntempfile='=3.27.0'\n",
        )
        .unwrap();
        std::fs::write(
            project.join("src/lib.rs"),
            "#[test]\nfn tempfile_uses_scratch() {\n    let file = tempfile::NamedTempFile::new().unwrap();\n    let scratch = std::path::PathBuf::from(std::env::var_os(\"TMPDIR\").unwrap());\n    assert!(file.path().starts_with(scratch));\n}\n",
        )
        .unwrap();
        let mut boundary = test_boundary(&std::fs::canonicalize(&project).unwrap());
        let linked_git = temp.path().join("linked-repository/.git");
        let linked_admin = linked_git.join("worktrees/project");
        std::fs::create_dir_all(&linked_admin).unwrap();
        boundary.git_writable = vec![linked_git.clone(), linked_admin.clone()];
        let registry = temp.path().join("registry-secret.json");
        std::fs::write(&registry, "must stay outside").unwrap();
        let home = linux_home().unwrap();
        let credential = home.join(".cargo/credentials.toml");
        let tool_paths = validated_tool_paths().unwrap();
        let expected_path = deterministic_path(&tool_paths).unwrap();

        assert_sandbox_clause(
            "inherited secrets are scrubbed",
            "test -z \"${OPENAI_API_KEY-}\" && test -z \"${RTRT_PARENT_PROJECT-}\"",
            &backend,
            &boundary,
            &tool_paths,
        );
        assert_sandbox_clause(
            "PATH is exact and deterministic",
            &format!(
                "test \"$PATH\" = '{}'",
                expected_path.replace('\'', "'\"'\"'")
            ),
            &backend,
            &boundary,
            &tool_paths,
        );
        assert_sandbox_clause(
            "host /tmp sibling is read-hidden",
            &format!("test ! -e {}", shell_quote(&registry)),
            &backend,
            &boundary,
            &tool_paths,
        );
        assert_sandbox_clause(
            "host /tmp sibling creation is denied",
            &format!("! printf tamper > {}", shell_quote(&registry)),
            &backend,
            &boundary,
            &tool_paths,
        );
        assert_sandbox_clause(
            "credentials and home data are hidden",
            &format!(
                "test ! -r {} && test ! -e {}",
                shell_quote(&credential),
                shell_quote(&home.join(".ssh"))
            ),
            &backend,
            &boundary,
            &tool_paths,
        );
        assert_sandbox_clause(
            "nested user namespaces are disabled",
            "! unshare -Ur true",
            &backend,
            &boundary,
            &tool_paths,
        );
        assert_sandbox_clause(
            "Cargo and Rust toolchain work offline",
            "cargo test --offline --quiet",
            &backend,
            &boundary,
            &tool_paths,
        );
        assert_sandbox_clause(
            "project is writable",
            "printf ok > result",
            &backend,
            &boundary,
            &tool_paths,
        );
        assert_eq!(
            std::fs::read_to_string(project.join("result")).unwrap(),
            "ok"
        );
        assert_sandbox_clause(
            "linked worktree Git destination is writable",
            &format!(
                "printf ok > {}",
                shell_quote(&linked_admin.join("sandbox-write"))
            ),
            &backend,
            &boundary,
            &tool_paths,
        );
        assert_eq!(
            std::fs::read_to_string(linked_admin.join("sandbox-write")).unwrap(),
            "ok"
        );
        let scratch_file = format!("rtrt-private-{}", std::process::id());
        let host_scratch_file = Path::new(PRIVATE_SCRATCH).join(&scratch_file);
        assert!(!host_scratch_file.exists());
        assert_sandbox_clause(
            "TMPDIR scratch is writable and private",
            &format!(
                "test \"$TMPDIR\" = {PRIVATE_SCRATCH} && printf private > \"$TMPDIR/{scratch_file}\""
            ),
            &backend,
            &boundary,
            &tool_paths,
        );
        assert!(!host_scratch_file.exists());
        assert_eq!(
            std::fs::read_to_string(registry).unwrap(),
            "must stay outside"
        );

        let home_fixture = tempfile::tempdir_in(linux_home().unwrap()).unwrap();
        let home_project = home_fixture.path().join("project");
        std::fs::create_dir(&home_project).unwrap();
        let home_boundary = test_boundary(&std::fs::canonicalize(&home_project).unwrap());
        let home_sibling = home_fixture.path().join("forbidden-sibling");
        assert_sandbox_clause(
            "normal home project child is writable but sibling creation is denied",
            &format!(
                "printf ok > project-write && ! printf denied > {}",
                shell_quote(&home_sibling)
            ),
            &backend,
            &home_boundary,
            &tool_paths,
        );
        assert_eq!(
            std::fs::read_to_string(home_project.join("project-write")).unwrap(),
            "ok"
        );
        assert!(!home_sibling.exists());
        assert_sandbox_clause(
            "network and /dev/tcp are unavailable",
            "! (exec 3<>/dev/tcp/127.0.0.1/9)",
            &backend,
            &boundary,
            &tool_paths,
        );
        assert_eq!(
            spawn_sandbox("exit 37", &backend, &boundary, &tool_paths).unwrap(),
            37
        );
    }
}
