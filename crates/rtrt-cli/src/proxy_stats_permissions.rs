use std::path::Path;

use anyhow::{Context, Result};

#[cfg(not(unix))]
pub(super) fn prepare_private_store(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn prepare_private_store(path: &Path) -> Result<()> {
    use std::fs::{self, DirBuilder, Permissions};
    use std::io::ErrorKind;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        // Only the default state directory is ours to change, never an override's parent.
        if std::env::var_os("RTRT_PROXY_STATS_PATH").is_none() {
            let metadata = match fs::symlink_metadata(parent) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    DirBuilder::new()
                        .mode(0o700)
                        .create(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                    fs::symlink_metadata(parent)
                        .with_context(|| format!("inspect {}", parent.display()))?
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("inspect {}", parent.display()));
                }
            };
            anyhow::ensure!(
                !metadata.file_type().is_symlink()
                    && metadata.is_dir()
                    && metadata.uid() == crate::unsafe_geteuid(),
                "not a real directory owned by current user: {}",
                parent.display()
            );
            if metadata.permissions().mode() & 0o7777 != 0o700 {
                fs::set_permissions(parent, Permissions::from_mode(0o700))
                    .with_context(|| format!("set private permissions on {}", parent.display()))?;
            }
        } else {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
    }

    prepare_private_file(path, true)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        prepare_private_file(Path::new(&sidecar), false)?;
    }
    Ok(())
}

#[cfg(unix)]
fn prepare_private_file(path: &Path, create: bool) -> Result<()> {
    use std::fs::{self, OpenOptions, Permissions};
    use std::io::ErrorKind;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if !create {
                return Ok(());
            }
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("create {}", path.display()))?;
            file.set_permissions(Permissions::from_mode(0o600))
                .with_context(|| format!("set private permissions on {}", path.display()))?;
            fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    let uid = crate::unsafe_geteuid();
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file() && metadata.uid() == uid,
        "not a real regular file owned by current user: {}",
        path.display()
    );
    if metadata.permissions().mode() & 0o7777 == 0o600 {
        return Ok(());
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    options.custom_flags(0x20_000);
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    options.custom_flags(0x100);
    let file = options
        .open(path)
        .with_context(|| format!("open without following symlinks: {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("inspect opened {}", path.display()))?;
    let current =
        fs::symlink_metadata(path).with_context(|| format!("reinspect {}", path.display()))?;
    anyhow::ensure!(
        opened.is_file()
            && opened.uid() == uid
            && !current.file_type().is_symlink()
            && current.is_file()
            && current.uid() == uid
            && opened.dev() == metadata.dev()
            && opened.ino() == metadata.ino()
            && opened.dev() == current.dev()
            && opened.ino() == current.ino(),
        "not a real regular file with stable identity and current owner: {}",
        path.display()
    );
    file.set_permissions(Permissions::from_mode(0o600))
        .with_context(|| format!("set private permissions on {}", path.display()))
}
