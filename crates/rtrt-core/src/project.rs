//! Project attribution for a working directory.
//!
//! Claude Code stores sessions under `~/.claude/projects/<encoded-cwd>/`, where
//! the project "bucket" historically defaulted to the *basename* of the cwd.
//! That is wrong for any capture taken in a sub-directory (`src`, `web`, `gui`,
//! …) or a git worktree: each one becomes its own bogus project even though the
//! real project is the enclosing git repository.
//!
//! [`project_for_cwd`] fixes this by attributing a cwd to the basename of its
//! **git repository root** instead of the cwd basename. It is a pure resolver:
//! it only reads `.git` entries while walking up the tree, never mutates state,
//! and never panics — on any IO/parse error it falls back to the cwd basename
//! (the previous behaviour), so attribution can only improve, never regress.

use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use sha2::{Digest, Sha256};

use crate::{Error, Result};

const RUNTIME_TMP_ENV_VAR: &str = "RTRT_TMP_DIR";
const STATE_DIR_NAME: &str = ".rtrt";
const TMP_DIR_NAME: &str = "tmp";
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
static ATOMIC_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

const PROJECT_ID_DOMAIN: &[u8] = b"rtrt.project-identity.v1\0";
const PROJECT_SLUG_MAX_LEN: usize = 96;
const CLAUDE_TMP_FINGERPRINT_LEN: usize = 16;
/// Smallest pathname capacity among supported Unix `sockaddr_un.sun_path`
/// layouts, including the terminating NUL (104 bytes on BSD/macOS).
pub const CLAUDE_AF_UNIX_PATH_MAX_BYTES: usize = 104;
/// Space retained for Claude's slash, generated bridge-socket basename, and
/// terminating NUL. Keeping this explicit prevents a safe temp root from later
/// becoming an unusable AF_UNIX pathname.
const CLAUDE_BRIDGE_SOCKET_SUFFIX_BUDGET: usize = 58;

/// Stable identity of one project, independent of its human-readable basename.
///
/// Linked worktrees have distinct `checkout_root`s but share the canonical
/// `memory_root`, fingerprint, and slug of their common main repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    checkout_root: PathBuf,
    memory_root: PathBuf,
    label: String,
    fingerprint: String,
    slug: String,
}

impl ProjectIdentity {
    /// Derive identity by inspecting filesystem Git control files only. No Git
    /// executable or repository-writable RTRT marker participates.
    pub fn derive(cwd: impl AsRef<Path>) -> Result<Self> {
        let cwd = fs::canonicalize(cwd.as_ref()).map_err(Error::Io)?;
        Ok(Self::derive_from(&cwd, cwd.ancestors()))
    }

    pub fn checkout_root(&self) -> &Path {
        &self.checkout_root
    }

    pub fn memory_root(&self) -> &Path {
        &self.memory_root
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn slug(&self) -> &str {
        &self.slug
    }

    fn derive_from<'a>(cwd: &Path, ancestors: impl Iterator<Item = &'a Path>) -> Self {
        let mut checkout_root = cwd.to_path_buf();
        let mut memory_root = cwd.to_path_buf();
        for ancestor in ancestors {
            let git = ancestor.join(".git");
            let Ok(metadata) = fs::symlink_metadata(&git) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                checkout_root = ancestor.to_path_buf();
                memory_root = checkout_root.clone();
                break;
            }
            if metadata.is_file() {
                checkout_root = ancestor.to_path_buf();
                memory_root = main_repo_root_from_gitfile(&git)
                    .and_then(|root| fs::canonicalize(root).ok())
                    .unwrap_or_else(|| checkout_root.clone());
                break;
            }
        }
        let label = basename(&memory_root).unwrap_or_else(|| "project".to_string());
        let fingerprint = project_fingerprint(&memory_root);
        let slug = project_slug(&label, &fingerprint);
        Self {
            checkout_root,
            memory_root,
            label,
            fingerprint,
            slug,
        }
    }
}

/// Strict operator-owned directory for this project.
pub fn project_storage_dir(identity: &ProjectIdentity) -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot determine operator home directory".to_string()))?;
    Ok(project_storage_dir_in(&home, identity))
}

/// Strict project directory below an explicit operator home. Useful to
/// services with an already-resolved home and to isolated tests.
pub fn project_storage_dir_in(home: &Path, identity: &ProjectIdentity) -> PathBuf {
    home.join(".rtrt").join("projects").join(identity.slug())
}

/// Strict project-bound memory database path. Repository configuration cannot
/// override this location.
pub fn project_memory_db_path(identity: &ProjectIdentity) -> Result<PathBuf> {
    Ok(project_storage_dir(identity)?.join("memory.sqlite"))
}

/// Strict project-bound DB path below an explicit operator home.
pub fn project_memory_db_path_in(home: &Path, identity: &ProjectIdentity) -> PathBuf {
    project_storage_dir_in(home, identity).join("memory.sqlite")
}

fn project_fingerprint(root: &Path) -> String {
    let mut hash = Sha256::new();
    hash.update(PROJECT_ID_DOMAIN);
    hash_path(&mut hash, root);
    let digest = hash.finalize();
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(unix)]
fn hash_path(hash: &mut Sha256, path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

#[cfg(windows)]
fn hash_path(hash: &mut Sha256, path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    hash.update((units.len() as u64).to_be_bytes());
    for unit in units {
        hash.update(unit.to_be_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn hash_path(hash: &mut Sha256, path: &Path) {
    let text = path.to_string_lossy();
    hash.update((text.len() as u64).to_be_bytes());
    hash.update(text.as_bytes());
}

fn project_slug(label: &str, fingerprint: &str) -> String {
    let label_limit = PROJECT_SLUG_MAX_LEN - 2 - fingerprint.len();
    let mut sanitized = String::with_capacity(label_limit);
    let mut separator = false;
    for character in label.chars() {
        let character = character.to_ascii_lowercase();
        if character.is_ascii_alphanumeric() {
            if separator && !sanitized.is_empty() && sanitized.len() < label_limit {
                sanitized.push('-');
            }
            separator = false;
            if sanitized.len() < label_limit {
                sanitized.push(character);
            }
        } else {
            separator = true;
        }
        if sanitized.len() >= label_limit {
            break;
        }
    }
    while sanitized.ends_with('-') {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        sanitized.push_str("project");
    }
    format!("{sanitized}--{fingerprint}")
}

/// Resolve the project name for a working directory.
///
/// Walks up from `cwd` (canonicalized if it exists, else used as-is). At each
/// ancestor it looks for a `.git` entry:
///
/// * `.git` is a **directory** — that ancestor is the repo root; the project is
///   its basename.
/// * `.git` is a **file** — this is a linked git worktree. The file contains
///   `gitdir: <path>`, where `<path>` is like `<main>/.git/worktrees/<wt>`. The
///   main repo root is the parent of that `.git` directory; the project is the
///   main repo's basename.
/// * No `.git` is found up to the filesystem root — fall back to the cwd
///   basename.
///
/// Never panics. Any IO or parse error falls back to the cwd basename.
pub fn project_for_cwd(cwd: &Path) -> String {
    let start = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    project_for_ancestors(&start, start.ancestors())
}

/// Core walk, parameterised over the ancestor sequence to examine.
///
/// Production always calls this with the *full*, unbounded `start.ancestors()`
/// (root markers anywhere above `start` legitimately win — see module docs).
/// Tests call it with a bounded, fixture-scoped ancestor list so their
/// assertions don't depend on what markers happen to exist above the system
/// temp dir on the machine running them.
fn project_for_ancestors<'a>(start: &Path, ancestors: impl Iterator<Item = &'a Path>) -> String {
    for ancestor in ancestors {
        let dot_git = ancestor.join(".git");
        let meta = match std::fs::symlink_metadata(&dot_git) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.is_dir() {
            // Normal repo: this ancestor is the root.
            return basename(ancestor).unwrap_or_else(|| basename_or_empty(start));
        }

        if meta.is_file() {
            // Linked worktree: resolve the main repo from the gitdir pointer.
            if let Some(main_root) = main_repo_root_from_gitfile(&dot_git) {
                if let Some(name) = basename(&main_root) {
                    return name;
                }
            }
            // Worktree pointer was unreadable/unparsable: this ancestor still
            // belongs to *some* repo, so prefer its basename over digging higher.
            return basename(ancestor).unwrap_or_else(|| basename_or_empty(start));
        }
    }

    // No `.git` found within the examined ancestors — fall back to basename.
    basename_or_empty(start)
}

/// Convenience wrapper over [`project_for_cwd`] taking a string path.
pub fn project_for_cwd_str(cwd: &str) -> String {
    project_for_cwd(Path::new(cwd))
}

/// Select RTRT's runtime scratch directory without reading environment state or
/// creating it.
///
/// A non-empty override wins, followed by the main project root, then the OS
/// temporary directory when no usable project root was found.
pub fn resolve_runtime_tmp_dir(
    tmp_override: Option<&OsStr>,
    project_root: Option<&Path>,
    os_tmp_dir: &Path,
) -> PathBuf {
    if let Some(tmp_override) = tmp_override.filter(|value| !value.is_empty()) {
        return PathBuf::from(tmp_override);
    }
    project_root
        .map(|root| root.join(STATE_DIR_NAME).join(TMP_DIR_NAME))
        .unwrap_or_else(|| os_tmp_dir.join(fallback_tmp_dir_name()))
}

/// Resolve and create RTRT's runtime scratch directory for the current process.
///
/// Once a project root is discovered, creation errors are returned rather than
/// silently redirecting runtime files to the OS temporary directory.
pub fn runtime_tmp_dir() -> io::Result<PathBuf> {
    let tmp_override = std::env::var_os(RUNTIME_TMP_ENV_VAR);
    let os_tmp_dir = std::env::temp_dir();
    let expected_owner = current_user_id()?;
    if let Some(tmp_override) = tmp_override.as_ref().filter(|value| !value.is_empty()) {
        return prepare_override_tmp_dir(Path::new(tmp_override), expected_owner);
    }

    if let Ok(cwd) = std::env::current_dir() {
        prepare_runtime_tmp_dir_for_cwd(&cwd, &os_tmp_dir, expected_owner)
    } else {
        prepare_fallback_tmp_dir(&os_tmp_dir, expected_owner)
    }
}

/// Create Claude Code's private bridge runtime directory outside repository
/// scratch space.
///
/// Claude's built-in sandbox creates AF_UNIX bridge sockets below `TMPDIR`.
/// Repository-scoped `.rtrt/tmp` paths (and inherited OpenCode `TMPDIR`s) can
/// exceed `sockaddr_un.sun_path`, so this deliberately uses a validated private
/// XDG runtime directory or canonical OS `/tmp`, with a stable
/// per-user/per-project leaf and an explicit socket-path budget. Unsafe existing
/// leaves fail closed rather than being repaired.
pub fn claude_runtime_tmp_dir(project_root: &Path) -> io::Result<PathBuf> {
    let identity = ProjectIdentity::derive(project_root).map_err(|error| match error {
        Error::Io(error) => error,
        other => io::Error::other(other.to_string()),
    })?;
    let expected_owner = current_user_id()?;

    #[cfg(unix)]
    {
        if let Some(xdg_runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            if let Ok(xdg_runtime) =
                validated_xdg_runtime_dir(Path::new(&xdg_runtime), expected_owner)
            {
                return prepare_claude_runtime_tmp_dir_in(
                    &xdg_runtime,
                    identity.fingerprint(),
                    expected_owner,
                );
            }
        }
        prepare_claude_runtime_tmp_dir_in(Path::new("/tmp"), identity.fingerprint(), expected_owner)
    }
    #[cfg(not(unix))]
    {
        prepare_claude_runtime_tmp_dir_in(
            &std::env::temp_dir(),
            identity.fingerprint(),
            expected_owner,
        )
    }
}

#[cfg(unix)]
fn validated_xdg_runtime_dir(
    xdg_runtime: &Path,
    expected_owner: Option<u32>,
) -> io::Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    validate_real_directory_components(xdg_runtime)?;
    let canonical = fs::canonicalize(xdg_runtime).map_err(|error| {
        path_error(
            error,
            "canonicalize XDG runtime directory for Claude",
            xdg_runtime,
        )
    })?;
    validate_real_directory_components(&canonical)?;
    let owner = expected_owner.ok_or_else(|| {
        unsafe_path_error("cannot determine XDG runtime directory owner", &canonical)
    })?;
    let metadata = fs::symlink_metadata(&canonical)?;
    if metadata.uid() != owner || metadata.permissions().mode() & 0o7777 != PRIVATE_DIR_MODE {
        return Err(unsafe_path_error(
            "XDG runtime directory is not private or has the wrong owner",
            &canonical,
        ));
    }
    Ok(canonical)
}

fn prepare_claude_runtime_tmp_dir_in(
    os_tmp: &Path,
    project_fingerprint: &str,
    expected_owner: Option<u32>,
) -> io::Result<PathBuf> {
    let canonical_tmp = fs::canonicalize(os_tmp)
        .map_err(|error| path_error(error, "canonicalize Claude OS temporary directory", os_tmp))?;
    validate_real_directory_components(&canonical_tmp)?;
    let candidate =
        claude_runtime_tmp_candidate(&canonical_tmp, project_fingerprint, expected_owner)?;
    let leaf = candidate
        .file_name()
        .ok_or_else(|| unsafe_path_error("Claude runtime candidate has no leaf", &candidate))?;
    ensure_confined_private_child(&canonical_tmp, leaf, expected_owner, false)
}

fn claude_runtime_tmp_candidate(
    canonical_tmp: &Path,
    project_fingerprint: &str,
    expected_owner: Option<u32>,
) -> io::Result<PathBuf> {
    let fingerprint = project_fingerprint
        .get(..CLAUDE_TMP_FINGERPRINT_LEN)
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| unsafe_path_error("invalid Claude project fingerprint", canonical_tmp))?;
    let owner = expected_owner.ok_or_else(|| {
        unsafe_path_error(
            "cannot determine Claude runtime directory owner",
            canonical_tmp,
        )
    })?;
    // Hex UID keeps the leaf bounded even at u32::MAX.
    let name = format!("rc-{owner:x}-{fingerprint}");
    let candidate = canonical_tmp.join(name);
    enforce_claude_socket_path_budget(&candidate)?;
    Ok(candidate)
}

#[cfg(all(test, unix))]
pub(crate) fn claude_runtime_tmp_candidate_for_test(
    os_tmp: &Path,
    project_fingerprint: &str,
    owner: u32,
) -> io::Result<PathBuf> {
    claude_runtime_tmp_candidate(&fs::canonicalize(os_tmp)?, project_fingerprint, Some(owner))
}

fn validate_real_directory_components(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(unsafe_path_error(
            "Claude runtime base is not absolute",
            path,
        ));
    }
    for component in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = fs::symlink_metadata(component)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(unsafe_path_error(
                "Claude runtime base contains a non-directory or symlink component",
                component,
            ));
        }
    }
    Ok(())
}

fn enforce_claude_socket_path_budget(path: &Path) -> io::Result<()> {
    let path_bytes = path_byte_len(path);
    let required = path_bytes
        .saturating_add(1) // slash
        .saturating_add(CLAUDE_BRIDGE_SOCKET_SUFFIX_BUDGET)
        .saturating_add(1); // terminating NUL
    if required > CLAUDE_AF_UNIX_PATH_MAX_BYTES {
        return Err(unsafe_path_error(
            "Claude runtime path leaves insufficient AF_UNIX bridge-socket budget",
            path,
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn path_byte_len(path: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().len()
}

#[cfg(not(unix))]
fn path_byte_len(path: &Path) -> usize {
    path.to_string_lossy().len()
}

#[cfg(test)]
fn runtime_tmp_dir_for_cwd(cwd: &Path, tmp_override: Option<&OsStr>, os_tmp_dir: &Path) -> PathBuf {
    let project_root = main_project_root_for_cwd(cwd);
    resolve_runtime_tmp_dir(tmp_override, project_root.as_deref(), os_tmp_dir)
}

fn prepare_runtime_tmp_dir_for_cwd(
    cwd: &Path,
    os_tmp_dir: &Path,
    expected_owner: Option<u32>,
) -> io::Result<PathBuf> {
    match main_project_root_for_cwd(cwd) {
        Some(root) => prepare_project_tmp_dir(&root, expected_owner),
        None => prepare_fallback_tmp_dir(os_tmp_dir, expected_owner),
    }
}

fn main_project_root_for_cwd(cwd: &Path) -> Option<PathBuf> {
    let start = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    for ancestor in start.ancestors() {
        let dot_git = ancestor.join(".git");
        let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
            continue;
        };

        if meta.is_dir() {
            return Some(ancestor.to_path_buf());
        }
        if meta.is_file() {
            // Submodules also use `.git` files, and malformed worktree pointers
            // must not redirect state into an unrelated ancestor or `/tmp`.
            return Some(
                main_repo_root_from_gitfile(&dot_git).unwrap_or_else(|| ancestor.to_path_buf()),
            );
        }
    }
    None
}

/// Given the path of a worktree `.git` *file*, return the main repository root.
///
/// Besides the `worktrees/<name>` layout, this validates Git's `commondir` and
/// `gitdir` backlink files. Submodule pointers (`.git/modules/...`) and
/// lookalike or malformed layouts therefore remain attributed to their current
/// checkout instead of redirecting state elsewhere.
fn main_repo_root_from_gitfile(git_file: &Path) -> Option<std::path::PathBuf> {
    let git_file_meta = fs::symlink_metadata(git_file).ok()?;
    if git_file_meta.file_type().is_symlink() || !git_file_meta.is_file() {
        return None;
    }

    let gitdir = control_path(git_file, "gitdir: ")?;
    let gitdir_meta = fs::symlink_metadata(&gitdir).ok()?;
    if gitdir_meta.file_type().is_symlink() || !gitdir_meta.is_dir() {
        return None;
    }
    let gitdir = fs::canonicalize(gitdir).ok()?;
    let worktrees_dir = gitdir.parent()?;
    if worktrees_dir.file_name()? != OsStr::new("worktrees") {
        return None;
    }
    let main_git_dir = worktrees_dir.parent()?;
    if main_git_dir.file_name()? != OsStr::new(".git") {
        return None;
    }
    let main_git_dir = fs::canonicalize(main_git_dir).ok()?;

    let common_dir = control_path(&gitdir.join("commondir"), "")?;
    if fs::canonicalize(common_dir).ok()? != main_git_dir {
        return None;
    }

    let backlink = control_path(&gitdir.join("gitdir"), "")?;
    let backlink_meta = fs::symlink_metadata(&backlink).ok()?;
    if backlink_meta.file_type().is_symlink() || !backlink_meta.is_file() {
        return None;
    }
    if fs::canonicalize(backlink).ok()? != fs::canonicalize(git_file).ok()? {
        return None;
    }

    let main_root = main_git_dir.parent()?;
    let main_dot_git = main_root.join(".git");
    let main_dot_git_meta = fs::symlink_metadata(&main_dot_git).ok()?;
    if main_dot_git_meta.file_type().is_symlink() || !main_dot_git_meta.is_dir() {
        return None;
    }
    (fs::canonicalize(main_dot_git).ok()? == main_git_dir).then(|| main_root.to_path_buf())
}

fn control_path(control_file: &Path, prefix: &str) -> Option<PathBuf> {
    let content = fs::read_to_string(control_file).ok()?;
    let mut lines = content.lines();
    let raw = lines.next()?.strip_prefix(prefix)?.trim();
    if raw.is_empty() || lines.any(|line| !line.trim().is_empty()) {
        return None;
    }
    let path = Path::new(raw);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        control_file.parent()?.join(path)
    })
}

fn prepare_project_tmp_dir(root: &Path, expected_owner: Option<u32>) -> io::Result<PathBuf> {
    let root = fs::canonicalize(root)
        .map_err(|error| path_error(error, "canonicalize project root", root))?;
    let metadata = fs::symlink_metadata(&root)?;
    if !metadata.is_dir() {
        return Err(unsafe_path_error("project root is not a directory", &root));
    }

    let state =
        ensure_confined_private_child(&root, OsStr::new(STATE_DIR_NAME), expected_owner, true)?;
    let tmp =
        ensure_confined_private_child(&state, OsStr::new(TMP_DIR_NAME), expected_owner, true)?;
    if !tmp.starts_with(&root) {
        return Err(unsafe_path_error(
            "runtime scratch directory escapes project root",
            &tmp,
        ));
    }
    Ok(tmp)
}

fn prepare_fallback_tmp_dir(os_tmp_dir: &Path, expected_owner: Option<u32>) -> io::Result<PathBuf> {
    let os_tmp_dir = fs::canonicalize(os_tmp_dir)
        .map_err(|error| path_error(error, "canonicalize OS temporary directory", os_tmp_dir))?;
    if !fs::symlink_metadata(&os_tmp_dir)?.is_dir() {
        return Err(unsafe_path_error(
            "OS temporary path is not a directory",
            &os_tmp_dir,
        ));
    }
    ensure_confined_private_child(
        &os_tmp_dir,
        &fallback_tmp_dir_name_for(expected_owner),
        expected_owner,
        true,
    )
}

fn prepare_override_tmp_dir(requested: &Path, expected_owner: Option<u32>) -> io::Result<PathBuf> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        std::env::current_dir()?.join(requested)
    };
    let mut cursor = absolute.clone();
    let mut missing = Vec::new();

    let canonical_anchor = loop {
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(unsafe_path_error(
                        "runtime scratch path component is not a real directory",
                        &cursor,
                    ));
                }
                break fs::canonicalize(&cursor).map_err(|error| {
                    path_error(error, "canonicalize runtime scratch path", &cursor)
                })?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(name) = cursor.file_name() else {
                    return Err(path_error(
                        error,
                        "locate existing runtime scratch parent",
                        &absolute,
                    ));
                };
                missing.push(name.to_os_string());
                if !cursor.pop() {
                    return Err(unsafe_path_error(
                        "runtime scratch path has no existing parent",
                        &absolute,
                    ));
                }
            }
            Err(error) => {
                return Err(path_error(error, "inspect runtime scratch path", &cursor));
            }
        }
    };

    if missing.is_empty() {
        return ensure_private_directory(&canonical_anchor, expected_owner, false);
    }

    let mut parent = canonical_anchor;
    for name in missing.into_iter().rev() {
        if Path::new(&name)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(unsafe_path_error(
                "runtime scratch path contains traversal",
                &absolute,
            ));
        }
        parent = ensure_confined_private_child(&parent, &name, expected_owner, true)?;
    }
    Ok(parent)
}

fn ensure_confined_private_child(
    canonical_parent: &Path,
    name: &OsStr,
    expected_owner: Option<u32>,
    repair_mode: bool,
) -> io::Result<PathBuf> {
    if Path::new(name)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(unsafe_path_error(
            "runtime scratch child is not a single path component",
            &canonical_parent.join(name),
        ));
    }
    let path = canonical_parent.join(name);
    let canonical = ensure_private_directory(&path, expected_owner, repair_mode)?;
    if canonical.parent() != Some(canonical_parent) {
        return Err(unsafe_path_error(
            "runtime scratch path escaped its canonical parent",
            &canonical,
        ));
    }
    Ok(canonical)
}

fn ensure_private_directory(
    path: &Path,
    expected_owner: Option<u32>,
    repair_mode: bool,
) -> io::Result<PathBuf> {
    let mut created = false;
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(PRIVATE_DIR_MODE);
            }
            match builder.create(path) {
                Ok(()) => created = true,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(path_error(
                        error,
                        "create private runtime scratch directory",
                        path,
                    ));
                }
            }
        }
        Err(error) => {
            return Err(path_error(
                error,
                "inspect private runtime scratch directory",
                path,
            ));
        }
    }

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(unsafe_path_error(
            "runtime scratch path is not a real directory",
            path,
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let expected_owner = expected_owner.ok_or_else(|| {
            unsafe_path_error("cannot determine runtime scratch directory owner", path)
        })?;
        if metadata.uid() != expected_owner {
            return Err(unsafe_path_error(
                "runtime scratch directory has the wrong owner",
                path,
            ));
        }
        if (created || repair_mode) && metadata.mode() & 0o7777 != PRIVATE_DIR_MODE {
            fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR_MODE)).map_err(
                |error| path_error(error, "secure runtime scratch directory permissions", path),
            )?;
        }
        let verified = fs::symlink_metadata(path)?;
        if verified.file_type().is_symlink()
            || !verified.is_dir()
            || verified.uid() != expected_owner
            || verified.mode() & 0o7777 != PRIVATE_DIR_MODE
        {
            return Err(unsafe_path_error(
                "runtime scratch directory failed owner or mode validation",
                path,
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (expected_owner, repair_mode, created);

    fs::canonicalize(path)
        .map_err(|error| path_error(error, "canonicalize runtime scratch directory", path))
}

/// Atomically replace a file inside a private runtime directory without ever
/// opening or truncating the destination path.
pub fn write_private_file_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| unsafe_path_error("private file path has no file name", path))?;
    let parent = path
        .parent()
        .ok_or_else(|| unsafe_path_error("private file path has no parent", path))?;
    let expected_owner = current_user_id()?;
    let parent = ensure_private_directory(parent, expected_owner, false)?;
    let target = parent.join(file_name);
    let sequence = ATOMIC_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        sequence
    ));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(PRIVATE_FILE_MODE);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| path_error(error, "create atomic private file", &temporary))?;
    let result = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};

            file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
            let metadata = file.metadata()?;
            let expected_owner = expected_owner.ok_or_else(|| {
                unsafe_path_error("cannot determine atomic private file owner", &temporary)
            })?;
            if metadata.uid() != expected_owner || metadata.mode() & 0o7777 != PRIVATE_FILE_MODE {
                return Err(unsafe_path_error(
                    "atomic private file failed owner or mode validation",
                    &temporary,
                ));
            }
        }
        file.write_all(contents)?;
        file.sync_data()?;
        drop(file);
        replace_file_no_follow(&temporary, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn replace_file_no_follow(temporary: &Path, target: &Path) -> io::Result<()> {
    fs::rename(temporary, target)
        .map_err(|error| path_error(error, "atomically replace private file", target))
}

fn fallback_tmp_dir_name() -> OsString {
    fallback_tmp_dir_name_for(current_user_id().ok().flatten())
}

fn fallback_tmp_dir_name_for(user_id: Option<u32>) -> OsString {
    if let Some(user_id) = user_id {
        return format!("rtrt-{user_id}").into();
    }

    #[cfg(not(unix))]
    {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        dirs::home_dir().hash(&mut hasher);
        return format!("rtrt-user-{:016x}", hasher.finish()).into();
    }
    #[cfg(unix)]
    format!("rtrt-process-{}", std::process::id()).into()
}

#[cfg(unix)]
fn current_user_id() -> io::Result<Option<u32>> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let status = fs::read_to_string("/proc/self/status")?;
        let effective = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|ids| ids.split_whitespace().nth(1))
            .and_then(|uid| uid.parse::<u32>().ok())
            .ok_or_else(|| io::Error::other("cannot parse effective uid from /proc/self/status"))?;
        Ok(Some(effective))
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let command = if Path::new("/usr/bin/id").is_file() {
            "/usr/bin/id"
        } else {
            "/bin/id"
        };
        let output = std::process::Command::new(command).arg("-u").output()?;
        if !output.status.success() {
            return Err(io::Error::other("id -u failed"));
        }
        let uid = String::from_utf8(output.stdout)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .ok_or_else(|| io::Error::other("cannot parse uid from id -u"))?;
        Ok(Some(uid))
    }
}

#[cfg(not(unix))]
fn current_user_id() -> io::Result<Option<u32>> {
    Ok(None)
}

fn unsafe_path_error(message: &str, path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{message}: {}", path.display()),
    )
}

fn path_error(error: io::Error, operation: &str, path: &Path) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{operation} {}: {error}", path.display()),
    )
}

/// Final path component as an owned `String`, if any.
fn basename(path: &Path) -> Option<String> {
    path.file_name().map(|os| os.to_string_lossy().into_owned())
}

/// Final path component, or the empty string for a root-only path.
fn basename_or_empty(path: &Path) -> String {
    basename(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique, self-cleaning temp directory rooted in the system temp dir.
    /// Avoids pulling in an external `tempfile` dependency while staying
    /// deterministic (unique per test via pid + monotonic counter).
    /// Canonical only where it matters. macOS reaches the temp dir through
    /// `/var -> /private/var`, which breaks comparisons against canonical paths.
    /// Windows canonicalization instead yields a `\\?\` verbatim path, which the
    /// production code rejects, so the plain temp path is the correct fixture there.
    fn canonical_for_tests(path: &std::path::Path) -> std::path::PathBuf {
        #[cfg(unix)]
        {
            std::fs::canonicalize(path).expect("canonicalize temp path")
        }
        #[cfg(not(unix))]
        {
            path.to_path_buf()
        }
    }

    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut base = std::env::temp_dir();
            base.push(format!(
                "rtrt-core-project-{}-{}-{}",
                std::process::id(),
                tag,
                n
            ));
            std::fs::create_dir_all(&base).expect("create temp dir");
            // macOS reaches the temp dir through `/var -> /private/var`, and the
            // functions under test return canonical paths, so the fixture has to
            // start canonical for the comparison to mean anything.
            let base = canonical_for_tests(&base);
            Self(base)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn create_linked_worktree(main: &Path, worktree: &Path, name: &str) {
        let gitdir = main.join(".git").join("worktrees").join(name);
        fs::create_dir_all(&gitdir).unwrap();
        fs::create_dir_all(worktree).unwrap();
        let dot_git = worktree.join(".git");
        fs::write(&dot_git, format!("gitdir: {}\n", gitdir.display())).unwrap();
        fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        fs::write(gitdir.join("gitdir"), format!("{}\n", dot_git.display())).unwrap();
    }

    #[test]
    fn project_identity_distinguishes_same_basename_clones() {
        let tmp = TmpDir::new("identity-clones");
        let first = tmp.path().join("one").join("repo");
        let second = tmp.path().join("two").join("repo");
        fs::create_dir_all(first.join(".git")).unwrap();
        fs::create_dir_all(second.join(".git")).unwrap();

        let first = ProjectIdentity::derive(&first).unwrap();
        let second = ProjectIdentity::derive(&second).unwrap();
        assert_eq!(first.label, second.label);
        assert_ne!(first.fingerprint, second.fingerprint);
        assert_ne!(first.slug, second.slug);
    }

    #[test]
    fn linked_worktrees_share_identity_but_not_checkout_root() {
        let tmp = TmpDir::new("identity-worktree");
        let main = tmp.path().join("main");
        let worktree = tmp.path().join("linked");
        create_linked_worktree(&main, &worktree, "linked");

        let main_identity = ProjectIdentity::derive(&main).unwrap();
        let linked_identity = ProjectIdentity::derive(&worktree).unwrap();
        assert_ne!(main_identity.checkout_root, linked_identity.checkout_root);
        assert_eq!(main_identity.memory_root, linked_identity.memory_root);
        assert_eq!(main_identity.fingerprint, linked_identity.fingerprint);
        assert_eq!(main_identity.slug, linked_identity.slug);
    }

    #[test]
    fn submodule_identity_stays_bound_to_submodule_checkout() {
        let tmp = TmpDir::new("identity-submodule");
        let parent = tmp.path().join("parent");
        fs::create_dir_all(parent.join(".git/modules/child")).unwrap();
        let child = parent.join("child");
        fs::create_dir(&child).unwrap();
        fs::write(child.join(".git"), "gitdir: ../.git/modules/child\n").unwrap();

        let identity = ProjectIdentity::derive(&child).unwrap();
        assert_eq!(identity.checkout_root, fs::canonicalize(&child).unwrap());
        assert_eq!(identity.memory_root, fs::canonicalize(&child).unwrap());
        assert_eq!(identity.label, "child");
    }

    #[test]
    fn non_git_identity_is_canonical_and_stable() {
        let tmp = TmpDir::new("identity-plain");
        let plain = tmp.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        let canonical = fs::canonicalize(&plain).unwrap();
        let bounded = canonical
            .ancestors()
            .take_while(|path| path.starts_with(tmp.path()));
        let first = ProjectIdentity::derive_from(&canonical, bounded);
        let bounded = canonical
            .ancestors()
            .take_while(|path| path.starts_with(tmp.path()));
        let second = ProjectIdentity::derive_from(&canonical, bounded);

        assert_eq!(first.checkout_root, canonical);
        assert_eq!(first.memory_root, canonical);
        assert_eq!(first, second);
    }

    #[test]
    fn project_slug_is_sanitized_and_bounded() {
        let fingerprint = "0123456789abcdef0123456789abcdef";
        let slug = project_slug(
            "  A Very_LONG.Project Name !!! with spaces and an excessively long tail repeated repeated repeated  ",
            fingerprint,
        );
        assert!(slug.len() <= PROJECT_SLUG_MAX_LEN);
        assert!(slug.ends_with(&format!("--{fingerprint}")));
        assert!(
            slug.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
        assert_eq!(
            project_slug("한글", fingerprint),
            format!("project--{fingerprint}")
        );
    }

    #[test]
    fn strict_storage_helpers_never_use_repository_state() {
        let tmp = TmpDir::new("identity-storage");
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join(".rtrt")).unwrap();
        let identity = ProjectIdentity::derive(&repo).unwrap();
        let home = tmp.path().join("home");

        assert_eq!(
            project_memory_db_path_in(&home, &identity),
            home.join(".rtrt/projects")
                .join(&identity.slug)
                .join("memory.sqlite")
        );
    }

    #[test]
    fn nested_subdir_resolves_to_repo_root() {
        let tmp = TmpDir::new("nested");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let nested = repo.join("crates").join("drivers").join("src");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(project_for_cwd(&nested), "repo");
        // The repo root itself also resolves to "repo".
        assert_eq!(project_for_cwd(&repo), "repo");
        // And the string convenience wrapper agrees.
        assert_eq!(project_for_cwd_str(nested.to_str().unwrap()), "repo");
    }

    #[test]
    fn worktree_resolves_to_main_repo_basename() {
        let tmp = TmpDir::new("worktree");
        let main = tmp.path().join("mainrepo");
        let wt = tmp.path().join("wt-checkout");
        create_linked_worktree(&main, &wt, "wt");

        assert_eq!(project_for_cwd(&wt), "mainrepo");
        let wt_sub = wt.join("crates").join("x");
        std::fs::create_dir_all(&wt_sub).unwrap();
        assert_eq!(project_for_cwd(&wt_sub), "mainrepo");
    }

    #[test]
    fn runtime_tmp_uses_normal_repo_root() {
        let tmp = TmpDir::new("runtime-normal");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let path = runtime_tmp_dir_for_cwd(&repo, None, &tmp.path().join("os-tmp"));

        assert_eq!(path, repo.join(".rtrt").join("tmp"));
        assert!(
            !path.exists(),
            "pure resolver must not create the directory"
        );
    }

    #[test]
    fn runtime_tmp_uses_repo_root_from_subdirectory() {
        let tmp = TmpDir::new("runtime-subdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let subdir = repo.join("crates").join("rtrt-core");
        std::fs::create_dir_all(&subdir).unwrap();

        assert_eq!(
            runtime_tmp_dir_for_cwd(&subdir, None, &tmp.path().join("os-tmp")),
            repo.join(".rtrt").join("tmp")
        );
    }

    #[test]
    fn runtime_tmp_uses_main_repo_root_from_linked_worktree() {
        let tmp = TmpDir::new("runtime-worktree");
        let main = tmp.path().join("mainrepo");
        let worktree = tmp.path().join("worktree");
        create_linked_worktree(&main, &worktree, "wt");
        let subdir = worktree.join("src");
        std::fs::create_dir_all(&subdir).unwrap();

        assert_eq!(
            runtime_tmp_dir_for_cwd(&subdir, None, &tmp.path().join("os-tmp")),
            main.join(".rtrt").join("tmp")
        );
    }

    #[test]
    fn runtime_tmp_nonempty_override_wins() {
        let tmp = TmpDir::new("runtime-override");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let override_dir = tmp.path().join("override");

        assert_eq!(
            runtime_tmp_dir_for_cwd(
                &repo,
                Some(override_dir.as_os_str()),
                &tmp.path().join("os-tmp"),
            ),
            override_dir
        );
    }

    #[test]
    fn runtime_tmp_empty_override_uses_project_root() {
        let tmp = TmpDir::new("runtime-empty-override");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        assert_eq!(
            runtime_tmp_dir_for_cwd(&repo, Some(OsStr::new("")), &tmp.path().join("os-tmp")),
            repo.join(".rtrt").join("tmp")
        );
    }

    #[test]
    fn runtime_tmp_without_project_uses_os_fallback() {
        let os_tmp = Path::new("/system/tmp");
        let expected = fallback_tmp_dir_name();

        assert_eq!(
            resolve_runtime_tmp_dir(None, None, os_tmp),
            os_tmp.join(expected)
        );
    }

    #[test]
    fn malformed_gitfile_keeps_current_checkout() {
        let tmp = TmpDir::new("malformed-gitfile");
        let checkout = tmp.path().join("checkout");
        let nested = checkout.join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(checkout.join(".git"), "gitdir: missing/admin\n").unwrap();

        assert_eq!(project_for_cwd(&nested), "checkout");
        assert_eq!(
            runtime_tmp_dir_for_cwd(&nested, None, &tmp.path().join("os-tmp")),
            checkout.join(".rtrt").join("tmp")
        );
    }

    #[test]
    fn worktree_shaped_gitfile_without_backlinks_keeps_current_checkout() {
        let tmp = TmpDir::new("worktree-lookalike");
        let main = tmp.path().join("main");
        let admin = main.join(".git").join("worktrees").join("fake");
        fs::create_dir_all(&admin).unwrap();
        let checkout = tmp.path().join("checkout");
        fs::create_dir(&checkout).unwrap();
        fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();

        assert_eq!(project_for_cwd(&checkout), "checkout");
        assert_eq!(
            runtime_tmp_dir_for_cwd(&checkout, None, &tmp.path().join("os-tmp")),
            checkout.join(".rtrt").join("tmp")
        );
    }

    #[test]
    fn submodule_gitfile_keeps_submodule_checkout() {
        let tmp = TmpDir::new("submodule-gitfile");
        let parent = tmp.path().join("parent");
        fs::create_dir_all(parent.join(".git").join("modules").join("child")).unwrap();
        let child = parent.join("child");
        let nested = child.join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(child.join(".git"), "gitdir: ../.git/modules/child\n").unwrap();

        assert_eq!(project_for_cwd(&nested), "child");
        assert_eq!(
            runtime_tmp_dir_for_cwd(&nested, None, &tmp.path().join("os-tmp")),
            child.join(".rtrt").join("tmp")
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_runtime_tmp_rejects_symlink_components() {
        use std::os::unix::fs::symlink;

        for component in [STATE_DIR_NAME, TMP_DIR_NAME] {
            let tmp = TmpDir::new(component);
            let repo = tmp.path().join("repo");
            fs::create_dir_all(repo.join(".git")).unwrap();
            let outside = tmp.path().join("outside");
            fs::create_dir_all(&outside).unwrap();
            if component == STATE_DIR_NAME {
                symlink(&outside, repo.join(STATE_DIR_NAME)).unwrap();
            } else {
                fs::create_dir(repo.join(STATE_DIR_NAME)).unwrap();
                symlink(&outside, repo.join(STATE_DIR_NAME).join(TMP_DIR_NAME)).unwrap();
            }

            let error = prepare_runtime_tmp_dir_for_cwd(
                &repo,
                &tmp.path().join("os-tmp"),
                current_user_id().unwrap(),
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(!outside.join(TMP_DIR_NAME).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn project_runtime_tmp_is_confined_and_mode_0700() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let tmp = TmpDir::new("private-project-dir");
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let state = repo.join(STATE_DIR_NAME);
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        let uid = current_user_id().unwrap().unwrap();

        let runtime =
            prepare_runtime_tmp_dir_for_cwd(&repo, &tmp.path().join("os-tmp"), Some(uid)).unwrap();
        let canonical_repo = fs::canonicalize(&repo).unwrap();

        assert!(runtime.starts_with(&canonical_repo));
        for directory in [&state, &runtime] {
            let metadata = fs::symlink_metadata(directory).unwrap();
            assert_eq!(metadata.uid(), uid);
            assert_eq!(metadata.mode() & 0o7777, PRIVATE_DIR_MODE);
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_rejects_wrong_owner() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TmpDir::new("wrong-owner");
        let directory = tmp.path().join("private");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(PRIVATE_DIR_MODE)).unwrap();
        let uid = current_user_id().unwrap().unwrap();
        let wrong_uid = if uid == u32::MAX { uid - 1 } else { uid + 1 };

        let error = ensure_private_directory(&directory, Some(wrong_uid), false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn fallback_is_per_user_private_and_not_shared_rtrt() {
        use std::os::unix::fs::MetadataExt;

        let tmp = TmpDir::new("private-fallback");
        let uid = current_user_id().unwrap().unwrap();
        let fallback = prepare_fallback_tmp_dir(tmp.path(), Some(uid)).unwrap();
        let metadata = fs::symlink_metadata(&fallback).unwrap();

        assert_eq!(
            fallback,
            fs::canonicalize(tmp.path())
                .unwrap()
                .join(format!("rtrt-{uid}"))
        );
        assert_ne!(fallback.file_name().unwrap(), OsStr::new("rtrt"));
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.mode() & 0o7777, PRIVATE_DIR_MODE);
    }

    #[cfg(unix)]
    #[test]
    fn fallback_rejects_non_directory_candidate() {
        let tmp = TmpDir::new("fallback-file");
        let uid = current_user_id().unwrap().unwrap();
        let candidate = tmp.path().join(format!("rtrt-{uid}"));
        fs::write(&candidate, "not a directory").unwrap();

        let error = prepare_fallback_tmp_dir(tmp.path(), Some(uid)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(candidate).unwrap(), "not a directory");
    }

    #[cfg(unix)]
    #[test]
    fn existing_override_requires_private_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let tmp = TmpDir::new("override-mode");
        let override_dir = tmp.path().join("override");
        fs::create_dir(&override_dir).unwrap();
        fs::set_permissions(&override_dir, fs::Permissions::from_mode(0o755)).unwrap();

        let error =
            prepare_override_tmp_dir(&override_dir, current_user_id().unwrap()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            fs::symlink_metadata(&override_dir).unwrap().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_runtime_is_short_private_stable_and_project_separated() {
        use std::os::unix::fs::MetadataExt;

        let uid = current_user_id().unwrap().unwrap();
        let first_fingerprint = format!("a{:015x}aaaaaaaaaaaaaaaa", std::process::id());
        let second_fingerprint = format!("b{:015x}bbbbbbbbbbbbbbbb", std::process::id());
        let canonical_tmp = fs::canonicalize("/tmp").unwrap();
        let first =
            claude_runtime_tmp_candidate(&canonical_tmp, &first_fingerprint, Some(uid)).unwrap();
        let repeated =
            claude_runtime_tmp_candidate(&canonical_tmp, &first_fingerprint, Some(uid)).unwrap();
        let second =
            claude_runtime_tmp_candidate(&canonical_tmp, &second_fingerprint, Some(uid)).unwrap();

        assert_eq!(first, repeated);
        assert_ne!(first, second);
        assert!(!first.starts_with(env!("CARGO_MANIFEST_DIR")));
        assert!(
            path_byte_len(&first) + 1 + CLAUDE_BRIDGE_SOCKET_SUFFIX_BUDGET
                < CLAUDE_AF_UNIX_PATH_MAX_BYTES
        );
        let fixture = TmpDir::new("claude-private");
        let private = ensure_confined_private_child(
            &fs::canonicalize(fixture.path()).unwrap(),
            first.file_name().unwrap(),
            Some(uid),
            false,
        )
        .unwrap();
        let metadata = fs::symlink_metadata(private).unwrap();
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.mode() & 0o7777, PRIVATE_DIR_MODE);
    }

    #[cfg(unix)]
    #[test]
    fn claude_runtime_rejects_symlink_and_wrong_mode_leaves() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let uid = current_user_id().unwrap().unwrap();
        let fixture = TmpDir::new("claude-unsafe-leaf");
        let canonical_tmp = fs::canonicalize(fixture.path()).unwrap();
        for (tag, symlink_leaf) in [("bad0", true), ("bad1", false)] {
            let fingerprint = format!("{:08x}{tag}0000aaaaaaaaaaaaaaaa", std::process::id());
            let leaf = canonical_tmp.join(format!(
                "rc-{uid:x}-{}",
                &fingerprint[..CLAUDE_TMP_FINGERPRINT_LEN]
            ));
            let _ = fs::remove_file(&leaf);
            let _ = fs::remove_dir(&leaf);
            if symlink_leaf {
                symlink(&canonical_tmp, &leaf).unwrap();
            } else {
                fs::create_dir(&leaf).unwrap();
                fs::set_permissions(&leaf, fs::Permissions::from_mode(0o755)).unwrap();
            }

            let error = ensure_confined_private_child(
                &canonical_tmp,
                leaf.file_name().unwrap(),
                Some(uid),
                false,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            if symlink_leaf {
                fs::remove_file(&leaf).unwrap();
            } else {
                fs::remove_dir(&leaf).unwrap();
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn claude_runtime_rejects_insufficient_socket_path_budget() {
        let tmp = TmpDir::new("claude-path-budget");
        let uid = current_user_id().unwrap().unwrap();
        let error = prepare_claude_runtime_tmp_dir_in(
            tmp.path(),
            "0123456789abcdef0123456789abcdef",
            Some(uid),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn no_git_falls_back_to_cwd_basename() {
        let tmp = TmpDir::new("nogit");
        let plain = tmp.path().join("lonely").join("leafdir");
        std::fs::create_dir_all(&plain).unwrap();

        // `project_for_cwd` walks all the way to the filesystem root, so a
        // stray `.git` anywhere above the system temp dir (this machine has
        // one at `/tmp/.git`) would legitimately win and make this assertion
        // depend on the environment. Bound the ancestor walk to the fixture
        // itself so the test only ever inspects directories it created,
        // proving the fallback fires with zero markers in scope — regardless
        // of what exists above `tmp.path()` on the machine running it.
        let bounded: Vec<&Path> = plain
            .ancestors()
            .take_while(|a| a.starts_with(tmp.path()))
            .collect();
        assert_eq!(
            project_for_ancestors(&plain, bounded.into_iter()),
            "leafdir"
        );
    }

    #[test]
    fn nonexistent_path_uses_basename_fallback() {
        // Path does not exist on disk and has no .git anywhere: must not panic,
        // and falls back to the cwd basename.
        let p = Path::new("/this/path/should/not/exist/rtrt-zzz/whatever");
        assert_eq!(project_for_cwd(p), "whatever");
    }

    #[test]
    fn root_only_path_does_not_panic() {
        // Resolving "/" must not panic; basename is empty.
        assert_eq!(project_for_cwd(Path::new("/")), "");
    }
}
