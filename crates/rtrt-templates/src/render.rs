use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    let root = target_dir.as_ref().to_path_buf();

    let files = template
        .files
        .iter()
        .map(|f| -> Result<RenderedFile> {
            Ok(RenderedFile {
                path: root.join(substitute(&f.path, &merged)?),
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
    std::fs::create_dir_all(&plan.root).map_err(Error::Io)?;
    for f in &plan.files {
        if let Some(parent) = f.path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        if !overwrite && f.path.exists() {
            return Err(Error::Config(format!(
                "refusing to overwrite existing file: {}",
                f.path.display()
            )));
        }
        std::fs::write(&f.path, &f.content).map_err(Error::Io)?;
        #[cfg(unix)]
        if f.executable {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&f.path).map_err(Error::Io)?.permissions();
            perm.set_mode(perm.mode() | 0o111);
            std::fs::set_permissions(&f.path, perm).map_err(Error::Io)?;
        }
    }
    Ok(())
}

/// Handlebars-backed `{{var}}` substitution. The base case (a single `{{key}}`
/// per slot) round-trips through the engine unchanged; more advanced templates
/// can also use Handlebars conditionals (`{{#if foo}}…{{/if}}`) and loops
/// (`{{#each items}}…{{/each}}`).
fn substitute(input: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    HBS.render_template(input, vars)
        .map_err(|e| Error::Config(format!("handlebars: {e}")))
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
}
