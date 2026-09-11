use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use handlebars::Handlebars;
use once_cell::sync::Lazy;
use rtrt_core::{Error, Result};

use crate::{Template, validate_vars};

pub struct RenderPlan {
    pub root: PathBuf,
    pub files: Vec<RenderedFile>,
    pub post_hooks: Vec<String>,
}

pub struct RenderedFile {
    pub path: PathBuf,
    pub content: String,
    pub executable: bool,
}

pub fn plan(
    template: &Template,
    target_dir: impl AsRef<Path>,
    vars: BTreeMap<String, String>,
) -> Result<RenderPlan> {
    validate_vars(template, &vars)?;
    let merged = crate::resolve_vars(template, vars);
    let root = secure_target_path(target_dir.as_ref())?;

    let files = template
        .files
        .iter()
        .map(|f| -> Result<RenderedFile> {
            let rendered_path = substitute(&f.path, &merged)?;
            validate_rendered_path(&rendered_path)?;
            Ok(RenderedFile {
                path: root.join(rendered_path),
                content: substitute(&f.content, &merged)?,
                executable: f.executable,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let post_hooks = template
        .post_hooks
        .iter()
        .map(|h| substitute(h, &merged))
        .collect::<Result<Vec<_>>>()?;

    Ok(RenderPlan {
        root,
        files,
        post_hooks,
    })
}

pub fn write(plan: &RenderPlan, overwrite: bool) -> Result<()> {
    let planned_root = secure_target_path(&plan.root)?;
    if planned_root != plan.root {
        return Err(Error::Config(
            "scaffold target changed after planning".to_string(),
        ));
    }
    create_secure_dir(&plan.root)?;
    let canonical_root = std::fs::canonicalize(&plan.root).map_err(Error::Io)?;
    if canonical_root != plan.root {
        return Err(Error::Config(
            "scaffold target resolved outside its planned path".to_string(),
        ));
    }
    for f in &plan.files {
        let relative = f.path.strip_prefix(&plan.root).map_err(|_| {
            Error::Config(format!(
                "rendered file must stay inside scaffold target: {}",
                f.path.display()
            ))
        })?;
        validate_rendered_path(&relative.to_string_lossy())?;
        if let Some(parent) = f.path.parent() {
            create_secure_dir(parent)?;
            let canonical_parent = std::fs::canonicalize(parent).map_err(Error::Io)?;
            if !canonical_parent.starts_with(&canonical_root) {
                return Err(Error::Config(format!(
                    "rendered file escapes scaffold target: {}",
                    f.path.display()
                )));
            }
        }
        if overwrite {
            write_replacing(&f.path, f.content.as_bytes(), f.executable)?;
        } else {
            write_exclusive(&f.path, f.content.as_bytes(), f.executable)?;
        }
    }
    Ok(())
}

fn write_exclusive(path: &Path, content: &[u8], executable: bool) -> Result<()> {
    let file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::Config(format!(
                "refusing to overwrite existing file: {}",
                path.display()
            )));
        }
        Err(error) => return Err(Error::Io(error)),
    };
    let mut cleanup = RemoveOnDrop::new(path.to_path_buf());
    write_open_file(file, content, executable)?;
    cleanup.keep();
    Ok(())
}

/// Replace `path`'s contents without ever writing *through* a symlink that
/// appears there. The bytes land in a sibling temporary file and `rename` swaps
/// it in, which replaces such a link on every supported platform rather than
/// following it; a pre-write check alone cannot guarantee that.
pub(crate) fn replace_contents_no_follow(path: &Path, content: &[u8]) -> Result<()> {
    let (temp_path, file) = create_sibling_temp(path)?;
    let mut cleanup = RemoveOnDrop::new(temp_path.clone());
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_file()
    {
        file.set_permissions(metadata.permissions())
            .map_err(Error::Io)?;
    }
    write_open_file(file, content, false)?;
    std::fs::rename(&temp_path, path).map_err(Error::Io)?;
    cleanup.keep();
    Ok(())
}

#[cfg(unix)]
pub(crate) fn write_replacing(path: &Path, content: &[u8], executable: bool) -> Result<()> {
    let (temp_path, file) = create_sibling_temp(path)?;
    let mut cleanup = RemoveOnDrop::new(temp_path.clone());

    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_file()
    {
        file.set_permissions(metadata.permissions())
            .map_err(Error::Io)?;
    }
    write_open_file(file, content, executable)?;
    std::fs::rename(&temp_path, path).map_err(Error::Io)?;
    cleanup.keep();
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn write_replacing(_path: &Path, _content: &[u8], _executable: bool) -> Result<()> {
    Err(Error::Config(
        "atomic scaffold file replacement is unsupported on this platform".to_string(),
    ))
}

fn write_open_file(mut file: File, content: &[u8], executable: bool) -> Result<()> {
    file.write_all(content).map_err(Error::Io)?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = file.metadata().map_err(Error::Io)?.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        file.set_permissions(permissions).map_err(Error::Io)?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn create_sibling_temp(path: &Path) -> Result<(PathBuf, File)> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config(format!("rendered file has no parent: {}", path.display())))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| Error::Config(format!("rendered file has no name: {}", path.display())))?;
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{}.rtrt-tmp-{}-{sequence}",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Err(Error::Config(format!(
        "could not create private temporary file beside {}",
        path.display()
    )))
}

struct RemoveOnDrop {
    path: Option<PathBuf>,
}

impl RemoveOnDrop {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn keep(&mut self) {
        self.path = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn validate_rendered_path(path: &str) -> Result<()> {
    let path_ref = Path::new(path);
    let drive_prefixed = path.as_bytes().get(1) == Some(&b':')
        && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
    if path.is_empty()
        || path.contains('\0')
        || path.contains('\\')
        || drive_prefixed
        || path_ref
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::Config(format!(
            "rendered template path must be relative and stay inside the scaffold target: {path}"
        )));
    }
    Ok(())
}

/// Resolve a possibly nonexistent target through its nearest existing parent.
/// Every existing component must be a real directory, never a symlink.
fn secure_target_path(target: &Path) -> Result<PathBuf> {
    if target.as_os_str().is_empty()
        || target
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Error::Config(format!(
            "invalid scaffold target: {}",
            target.display()
        )));
    }
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir().map_err(Error::Io)?.join(target)
    };
    // Check the full lexical path first so dangling symlinks are rejected too;
    // `Path::exists` deliberately follows them and would otherwise miss them.
    reject_symlink_components(&absolute)?;
    let mut existing = absolute.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            Error::Config(format!("invalid scaffold target: {}", target.display()))
        })?;
        suffix.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| {
            Error::Config(format!("invalid scaffold target: {}", target.display()))
        })?;
    }
    reject_symlink_components(existing)?;
    let mut resolved = std::fs::canonicalize(existing).map_err(Error::Io)?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    if resolved.parent().is_none() {
        return Err(Error::Config(
            "filesystem root is not a scaffold target".to_string(),
        ));
    }
    Ok(resolved)
}

fn reject_symlink_components(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if std::fs::symlink_metadata(&current)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(Error::Config(format!(
                "symlink component is not allowed in scaffold path: {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn create_secure_dir(path: &Path) -> Result<()> {
    let expected = secure_target_path(path)?;
    let mut missing = Vec::new();
    let mut existing = expected.as_path();
    while !existing.exists() {
        missing.push(existing.to_path_buf());
        existing = existing.parent().ok_or_else(|| {
            Error::Config(format!("invalid scaffold directory: {}", path.display()))
        })?;
    }
    reject_symlink_components(existing)?;
    for directory in missing.iter().rev() {
        std::fs::create_dir(directory).map_err(Error::Io)?;
        let metadata = std::fs::symlink_metadata(directory).map_err(Error::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::Config(format!(
                "scaffold path component is not a directory: {}",
                directory.display()
            )));
        }
    }
    let canonical = std::fs::canonicalize(path).map_err(Error::Io)?;
    if canonical != expected {
        return Err(Error::Config(format!(
            "scaffold directory escaped while being created: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Handlebars-backed `{{var}}` substitution. The base case (a single `{{key}}`
/// per slot) round-trips through the engine unchanged; more advanced templates
/// can also use Handlebars conditionals (`{{#if foo}}…{{/if}}`) and loops
/// (`{{#each items}}…{{/each}}`).
///
/// Public so callers outside the template scaffolder (e.g. `rtrt-mcp`'s
/// `prompts/get` handler) can render the same way without copying the
/// engine config.
pub fn render_str(input: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    HBS.render_template(input, vars)
        .map_err(|e| Error::Config(format!("handlebars: {e}")))
}

/// Internal alias kept for the existing call sites in this module.
fn substitute(input: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    render_str(input, vars)
}

static HBS: Lazy<Handlebars<'static>> = Lazy::new(|| {
    let mut h = Handlebars::new();
    h.set_strict_mode(false);
    h.register_escape_fn(handlebars::no_escape);
    h
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TemplateCategory, TemplateFile, TemplateSource};

    fn template(path: &str) -> Template {
        Template {
            name: "test".into(),
            description: "test".into(),
            source: TemplateSource::Custom,
            category: TemplateCategory::Development,
            variables: vec![],
            files: vec![TemplateFile {
                path: path.into(),
                content: "content".into(),
                executable: false,
            }],
            post_hooks: vec![],
        }
    }

    /// Canonical only where it matters. macOS reaches the temp dir through
    /// `/var -> /private/var`, and the path validators reject symlink components.
    /// Windows canonicalization instead yields a `\\?\` verbatim path, so the plain
    /// temp path is the correct fixture there.
    fn canonical_temp_root() -> std::path::PathBuf {
        let base = std::env::temp_dir();
        #[cfg(unix)]
        {
            std::fs::canonicalize(base).expect("canonicalize temp dir")
        }
        #[cfg(not(unix))]
        {
            base
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let path = canonical_temp_root().join(format!(
            "rtrt-render-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn substitution_replaces_vars() {
        let mut vars = BTreeMap::new();
        vars.insert("name".into(), "foo".into());
        let out = substitute("hello {{name}}", &vars).unwrap();
        assert_eq!(out, "hello foo");
    }

    #[test]
    fn missing_var_yields_empty_in_non_strict_mode() {
        let vars: BTreeMap<String, String> = BTreeMap::new();
        let out = substitute("hello {{name}}", &vars).unwrap();
        assert_eq!(out, "hello ");
    }

    #[test]
    fn conditional_block() {
        let mut vars = BTreeMap::new();
        vars.insert("name".into(), "foo".into());
        let out = substitute("{{#if name}}hi {{name}}{{/if}}", &vars).unwrap();
        assert_eq!(out, "hi foo");
    }

    #[test]
    fn rendered_paths_reject_substituted_traversal_and_platform_prefixes() {
        let target = temp_dir("bad-rendered-path");
        for value in [
            "../escape",
            "/absolute",
            "C:\\escape",
            "C:escape",
            "..\\escape",
        ] {
            let mut vars = BTreeMap::new();
            vars.insert("output".into(), value.into());
            assert!(
                plan(&template("{{output}}/file.txt"), &target, vars).is_err(),
                "accepted {value}"
            );
        }
        std::fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn nonexistent_target_resolves_from_canonical_existing_parent() {
        let parent = temp_dir("nonexistent");
        let target = parent.join("new").join("nested");
        let plan = plan(&template("file.txt"), &target, BTreeMap::new()).unwrap();
        assert_eq!(
            plan.root,
            std::fs::canonicalize(&parent).unwrap().join("new/nested")
        );
        write(&plan, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("file.txt")).unwrap(),
            "content"
        );
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn non_overwrite_exclusively_preserves_existing_file() {
        let target = temp_dir("exclusive");
        let destination = target.join("file.txt");
        std::fs::write(&destination, "original").unwrap();
        let plan = plan(&template("file.txt"), &target, BTreeMap::new()).unwrap();

        assert!(write(&plan, false).is_err());
        assert_eq!(std::fs::read_to_string(destination).unwrap(), "original");
        std::fs::remove_dir_all(target).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_atomically_replaces_existing_file() {
        let target = temp_dir("replace");
        let destination = target.join("file.txt");
        std::fs::write(&destination, "original").unwrap();
        let plan = plan(&template("file.txt"), &target, BTreeMap::new()).unwrap();

        write(&plan, true).unwrap();
        assert_eq!(std::fs::read_to_string(destination).unwrap(), "content");
        assert!(std::fs::read_dir(&target).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".rtrt-tmp-")
        }));
        std::fs::remove_dir_all(target).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_cleans_temporary_file_when_replacement_fails() {
        let target = temp_dir("replace-error");
        std::fs::create_dir(target.join("file.txt")).unwrap();
        let plan = plan(&template("file.txt"), &target, BTreeMap::new()).unwrap();

        assert!(write(&plan, true).is_err());
        assert!(std::fs::read_dir(&target).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".rtrt-tmp-")
        }));
        std::fs::remove_dir_all(target).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn write_rejects_symlink_components_and_replaces_destination_symlink() {
        use std::os::unix::fs::symlink;

        let parent = temp_dir("symlink");
        let outside = temp_dir("outside");
        symlink(&outside, parent.join("linked")).unwrap();
        assert!(
            plan(
                &template("file.txt"),
                parent.join("linked"),
                BTreeMap::new()
            )
            .is_err()
        );

        let target = parent.join("target");
        let plan = plan(&template("nested/file.txt"), &target, BTreeMap::new()).unwrap();
        std::fs::create_dir(&target).unwrap();
        symlink(&outside, target.join("nested")).unwrap();
        assert!(write(&plan, true).is_err());

        std::fs::remove_file(target.join("nested")).unwrap();
        std::fs::create_dir(target.join("nested")).unwrap();
        let outside_file = outside.join("escaped.txt");
        std::fs::write(&outside_file, "outside").unwrap();
        let destination = target.join("nested/file.txt");
        symlink(&outside_file, &destination).unwrap();
        assert!(write(&plan, false).is_err());
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "outside");
        write(&plan, true).unwrap();
        assert!(
            !std::fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(destination).unwrap(), "content");
        assert_eq!(std::fs::read_to_string(outside_file).unwrap(), "outside");
        std::fs::remove_dir_all(parent).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }
}
