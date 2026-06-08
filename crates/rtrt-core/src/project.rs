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

use std::path::Path;

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

    for ancestor in start.ancestors() {
        let dot_git = ancestor.join(".git");
        let meta = match std::fs::symlink_metadata(&dot_git) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.is_dir() {
            // Normal repo: this ancestor is the root.
            return basename(ancestor).unwrap_or_else(|| basename_or_empty(&start));
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
            return basename(ancestor).unwrap_or_else(|| basename_or_empty(&start));
        }
    }

    // No `.git` anywhere up the tree — original fallback behaviour.
    basename_or_empty(&start)
}

/// Convenience wrapper over [`project_for_cwd`] taking a string path.
pub fn project_for_cwd_str(cwd: &str) -> String {
    project_for_cwd(Path::new(cwd))
}

/// Given the path of a worktree `.git` *file*, return the main repository root.
///
/// The file's content is `gitdir: <main>/.git/worktrees/<wt>`. The main repo
/// root is the parent of the `.git` directory that gitdir points into, i.e.
/// three levels up from `<wt>` (`<wt>` -> `worktrees` -> `.git` -> `<main>`).
/// Relative gitdir paths are resolved against the worktree directory.
fn main_repo_root_from_gitfile(git_file: &Path) -> Option<std::path::PathBuf> {
    let content = std::fs::read_to_string(git_file).ok()?;
    let raw = content
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))?
        .trim();
    if raw.is_empty() {
        return None;
    }

    let gitdir = Path::new(raw);
    // Resolve relative pointers against the worktree dir (parent of the file).
    let gitdir = if gitdir.is_absolute() {
        gitdir.to_path_buf()
    } else {
        git_file.parent()?.join(gitdir)
    };

    // gitdir = <main>/.git/worktrees/<wt>
    //   parent           -> <main>/.git/worktrees
    //   parent           -> <main>/.git
    //   parent           -> <main>
    let main_root = gitdir.parent()?.parent()?.parent()?;
    Some(std::fs::canonicalize(main_root).unwrap_or_else(|_| main_root.to_path_buf()))
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique, self-cleaning temp directory rooted in the system temp dir.
    /// Avoids pulling in an external `tempfile` dependency while staying
    /// deterministic (unique per test via pid + monotonic counter).
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

        // Main repo with a standard .git dir.
        let main = tmp.path().join("mainrepo");
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("wt")).unwrap();

        // Linked worktree dir living elsewhere, with a `.git` FILE pointer.
        let wt = tmp.path().join("wt-checkout");
        std::fs::create_dir_all(&wt).unwrap();
        let gitdir = main.join(".git").join("worktrees").join("wt");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();

        // From the worktree root and from a subdir within it.
        assert_eq!(project_for_cwd(&wt), "mainrepo");
        let wt_sub = wt.join("crates").join("x");
        std::fs::create_dir_all(&wt_sub).unwrap();
        assert_eq!(project_for_cwd(&wt_sub), "mainrepo");
    }

    #[test]
    fn no_git_falls_back_to_cwd_basename() {
        let tmp = TmpDir::new("nogit");
        let plain = tmp.path().join("lonely").join("leafdir");
        std::fs::create_dir_all(&plain).unwrap();

        assert_eq!(project_for_cwd(&plain), "leafdir");
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
