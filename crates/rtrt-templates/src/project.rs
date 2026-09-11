//! Reusable project-standardization lifecycle helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use rtrt_core::{Error, Result};

use crate::{Template, TemplateFile, builtin, render};

/// First managed standardization section number.
pub const FIRST_MANAGED_SECTION: u8 = 1;

/// Last managed standardization section number.
pub const LAST_MANAGED_SECTION: u8 = 7;

/// Relative repository path for the managed project contract.
pub const CONTRACT_PATH: &str = "CLAUDE.md";

/// Relative repository directory for managed project agents.
pub const AGENTS_DIR: &str = ".claude/agents";

/// Status for one managed `CLAUDE.md` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionStatus {
    /// Numeric section id from the managed `## N. Title` heading.
    pub number: u8,
    /// Canonical section title from the standardization template.
    pub title: String,
    /// Whether the section number is present in the target contract.
    pub present: bool,
    /// Whether a present section heading differs from the canonical title.
    pub stale: bool,
}

/// Status for one managed `.claude/agents/*.md` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    /// Agent file stem, without the `.md` suffix.
    pub name: String,
    /// Repository-relative path to the agent definition.
    pub path: PathBuf,
    /// Whether the managed agent file is present.
    pub present: bool,
}

/// Read-only inspection result for a standardized project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInspection {
    /// Canonical repository root used for inspection.
    pub root: PathBuf,
    /// Whether the managed project contract exists.
    pub contract_present: bool,
    /// Managed section statuses in numeric order.
    pub sections: Vec<SectionStatus>,
    /// Managed project-agent statuses in template order.
    pub managed_agents: Vec<AgentStatus>,
    /// Any project-local agent files found under `.claude/agents`.
    pub present_agents: Vec<String>,
    /// Numbered `## N.` sections outside the managed section range.
    pub extra_sections: Vec<u8>,
    /// Duplicate managed section numbers found in the contract.
    pub duplicate_sections: Vec<u8>,
}

/// One append-only or create-only repair action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairAction {
    /// Create the missing managed project contract.
    CreateContract {
        /// Repository-relative path that will be created.
        path: PathBuf,
    },
    /// Append one missing managed section to the existing contract.
    AppendSection {
        /// Managed section number.
        number: u8,
        /// Canonical section title.
        title: String,
    },
    /// Create one missing managed project-agent definition.
    InstallAgent {
        /// Repository-relative path that will be created.
        path: PathBuf,
    },
}

/// Planned repair actions and the rendered file content needed to apply them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRepairPlan {
    /// Canonical repository root used for repair.
    pub root: PathBuf,
    /// Ordered repair actions.
    pub actions: Vec<RepairAction>,
    contract_content: String,
    missing_sections: Vec<RenderedSection>,
    missing_agents: Vec<(PathBuf, String)>,
}

/// One exact-owned legacy orchestration asset eligible for retirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyOrchestrationRetirementAction {
    /// Remove the legacy Agent Teams section while preserving the rest of the contract.
    RemoveContractSection { path: PathBuf, backup: PathBuf },
    /// Remove the legacy tech-lead agent definition.
    RemoveAgent { path: PathBuf, backup: PathBuf },
}

/// Preflighted retirement of exact-owned legacy orchestration bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyOrchestrationRetirementPlan {
    /// Ordered actions that a refresh will apply.
    pub actions: Vec<LegacyOrchestrationRetirementAction>,
    root: PathBuf,
    files: Vec<LegacyRetirementFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LegacyRetirementFile {
    path: PathBuf,
    backup: PathBuf,
    original: Vec<u8>,
    replacement: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderedSection {
    number: u8,
    title: String,
    content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSection {
    number: u8,
    title: String,
    content: String,
}

/// Validate and canonicalize a user-supplied project path.
pub fn validate_project_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    if path.as_os_str().is_empty() {
        return Err(Error::Config("project path is empty".into()));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Error::Config(format!(
            "project path must not contain '..': {}",
            path.display()
        )));
    }
    let canonical = path.canonicalize().map_err(Error::Io)?;
    if !canonical.is_dir() {
        return Err(Error::Config(format!(
            "project path is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

/// Return the built-in standardization template used as lifecycle source of truth.
pub fn standardization_template() -> Result<Template> {
    builtin::ALL
        .iter()
        .find(|template| template.name == "standardization")
        .cloned()
        .ok_or_else(|| Error::Config("built-in standardization template is unavailable".into()))
}

/// Return a project-contract template by name.
pub fn contract_template(name: &str) -> Result<Template> {
    crate::find(name).ok_or_else(|| Error::Config(format!("unknown template: {name}")))
}

/// Return managed section metadata from the rendered standardization contract.
pub fn expected_sections(root: impl AsRef<Path>) -> Result<Vec<(u8, String)>> {
    let rendered = rendered_standardization(root)?;
    Ok(parse_sections(&rendered.contract_content)
        .into_iter()
        .map(|section| (section.number, section.title))
        .collect())
}

/// Return managed project-agent file paths from the standardization template.
pub fn expected_agents() -> Result<Vec<PathBuf>> {
    let template = standardization_template()?;
    Ok(template
        .files
        .iter()
        .filter_map(agent_file_path)
        .map(PathBuf::from)
        .collect())
}

/// Inspect a project for standardization contract and managed agents.
pub fn inspect_project(root: impl AsRef<Path>) -> Result<ProjectInspection> {
    inspect_project_with_vars(root, "standardization", BTreeMap::new())
}

/// Inspect a project using a named project-contract template and explicit vars.
pub fn inspect_project_with_vars(
    root: impl AsRef<Path>,
    template_name: &str,
    vars: BTreeMap<String, String>,
) -> Result<ProjectInspection> {
    let root = validate_project_path(root)?;
    let rendered = rendered_contract_template(&root, template_name, vars)?;
    let expected = parse_sections(&rendered.contract_content);
    let expected_by_number = expected
        .iter()
        .map(|section| (section.number, section))
        .collect::<BTreeMap<_, _>>();
    let contract_path = root.join(CONTRACT_PATH);
    let contract_present = contract_path.exists();
    let found = if contract_present {
        let raw = std::fs::read_to_string(&contract_path).map_err(Error::Io)?;
        parse_sections(&raw)
    } else {
        Vec::new()
    };
    let mut found_by_number: BTreeMap<u8, Vec<&ParsedSection>> = BTreeMap::new();
    for section in &found {
        found_by_number
            .entry(section.number)
            .or_default()
            .push(section);
    }

    let sections = expected
        .iter()
        .map(|section| {
            let found_sections = found_by_number.get(&section.number);
            let present = found_sections.is_some_and(|items| !items.is_empty());
            let stale = found_sections
                .and_then(|items| items.first())
                .is_some_and(|found| found.title != section.title);
            SectionStatus {
                number: section.number,
                title: section.title.clone(),
                present,
                stale,
            }
        })
        .collect();

    let managed_agents = rendered
        .agent_files
        .iter()
        .map(|(path, _content)| AgentStatus {
            name: agent_name(path),
            path: path.clone(),
            present: root.join(path).exists(),
        })
        .collect();

    let present_agents = present_agent_names(&root)?;
    let managed_numbers = expected_by_number.keys().copied().collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    let mut extra = BTreeSet::new();
    for section in &found {
        if !managed_numbers.contains(&section.number) {
            extra.insert(section.number);
        }
        if managed_numbers.contains(&section.number) && !seen.insert(section.number) {
            duplicates.insert(section.number);
        }
    }

    Ok(ProjectInspection {
        root,
        contract_present,
        sections,
        managed_agents,
        present_agents,
        extra_sections: extra.into_iter().collect(),
        duplicate_sections: duplicates.into_iter().collect(),
    })
}

/// Build a create-only and append-only repair plan for missing managed content.
pub fn plan_repair(root: impl AsRef<Path>) -> Result<ProjectRepairPlan> {
    plan_repair_with_vars(root, "standardization", BTreeMap::new())
}

/// Build a repair plan from a named project-contract template and explicit vars.
pub fn plan_repair_with_vars(
    root: impl AsRef<Path>,
    template_name: &str,
    vars: BTreeMap<String, String>,
) -> Result<ProjectRepairPlan> {
    let root = validate_project_path(root)?;
    let rendered = rendered_contract_template(&root, template_name, vars.clone())?;
    let inspection = inspect_project_with_vars(&root, template_name, vars)?;
    let mut actions = Vec::new();
    let mut missing_sections = Vec::new();
    let contract_rel = PathBuf::from(CONTRACT_PATH);

    if !inspection.contract_present {
        actions.push(RepairAction::CreateContract {
            path: contract_rel.clone(),
        });
    } else {
        let expected_sections = parse_sections(&rendered.contract_content);
        for status in inspection
            .sections
            .iter()
            .filter(|section| !section.present)
        {
            if let Some(section) = expected_sections
                .iter()
                .find(|expected| expected.number == status.number)
            {
                actions.push(RepairAction::AppendSection {
                    number: section.number,
                    title: section.title.clone(),
                });
                missing_sections.push(RenderedSection {
                    number: section.number,
                    title: section.title.clone(),
                    content: section.content.clone(),
                });
            }
        }
    }

    let mut missing_agents = Vec::new();
    for status in inspection
        .managed_agents
        .iter()
        .filter(|agent| !agent.present)
    {
        if let Some((path, content)) = rendered
            .agent_files
            .iter()
            .find(|(path, _content)| path == &status.path)
        {
            actions.push(RepairAction::InstallAgent { path: path.clone() });
            missing_agents.push((path.clone(), content.clone()));
        }
    }

    Ok(ProjectRepairPlan {
        root,
        actions,
        contract_content: rendered.contract_content,
        missing_sections,
        missing_agents,
    })
}

/// Apply a project repair plan without overwriting existing managed content.
pub fn apply_repair(plan: &ProjectRepairPlan) -> Result<()> {
    let contract_path = plan.root.join(CONTRACT_PATH);
    // If CLAUDE.md is a symlink (e.g. a prior external tool pointing it at a
    // private store), materialize the resolved content into a real file first.
    // rtrt then manages a normal repo file instead of writing through the link.
    if std::fs::symlink_metadata(&contract_path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        let resolved = std::fs::read_to_string(&contract_path).unwrap_or_default();
        std::fs::remove_file(&contract_path).map_err(Error::Io)?;
        std::fs::write(&contract_path, resolved).map_err(Error::Io)?;
    }
    if !contract_path.exists() {
        std::fs::write(&contract_path, &plan.contract_content).map_err(Error::Io)?;
    } else if !plan.missing_sections.is_empty() {
        let mut append = String::new();
        for section in &plan.missing_sections {
            if !append.is_empty() {
                append.push('\n');
            }
            append.push_str(section.content.trim_end());
            append.push('\n');
        }
        let mut existing = std::fs::OpenOptions::new()
            .append(true)
            .open(&contract_path)
            .map_err(Error::Io)?;
        use std::io::Write as _;
        existing.write_all(b"\n").map_err(Error::Io)?;
        existing.write_all(append.as_bytes()).map_err(Error::Io)?;
    }

    for (path, content) in &plan.missing_agents {
        let full_path = plan.root.join(path);
        if full_path.exists() {
            continue;
        }
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        std::fs::write(&full_path, content).map_err(Error::Io)?;
    }

    Ok(())
}

/// Plan removal only for byte-exact legacy orchestration assets owned by rtrt.
pub fn plan_legacy_orchestration_retirement(
    root: impl AsRef<Path>,
) -> Result<LegacyOrchestrationRetirementPlan> {
    let root = validate_project_path(root)?;
    let mut actions = Vec::new();
    let mut files = Vec::new();

    let contract_path = root.join(CONTRACT_PATH);
    if let Some(original) = read_regular_file_without_symlinks(&root, &contract_path)?
        && let Some(replacement) = remove_exact_legacy_section(&original)
    {
        let backup = backup_path(&contract_path);
        validate_backup(&backup, &original)?;
        actions.push(LegacyOrchestrationRetirementAction::RemoveContractSection {
            path: contract_path.clone(),
            backup: backup.clone(),
        });
        files.push(LegacyRetirementFile {
            path: contract_path,
            backup,
            original,
            replacement: Some(replacement),
        });
    }

    let agent_path = root.join(AGENTS_DIR).join("tech-lead.md");
    if let Some(original) = read_regular_file_without_symlinks(&root, &agent_path)?
        && original == builtin::LEGACY_STANDARDIZATION_TECH_LEAD.as_bytes()
    {
        let backup = backup_path(&agent_path);
        validate_backup(&backup, &original)?;
        actions.push(LegacyOrchestrationRetirementAction::RemoveAgent {
            path: agent_path.clone(),
            backup: backup.clone(),
        });
        files.push(LegacyRetirementFile {
            path: agent_path,
            backup,
            original,
            replacement: None,
        });
    }

    Ok(LegacyOrchestrationRetirementPlan {
        actions,
        root,
        files,
    })
}

/// Back up and retire every preflighted exact-owned legacy asset.
pub fn apply_legacy_orchestration_retirement(
    plan: &LegacyOrchestrationRetirementPlan,
) -> Result<()> {
    apply_legacy_orchestration_retirement_with_staging_hook(plan, || Ok(()))
}

fn apply_legacy_orchestration_retirement_with_staging_hook(
    plan: &LegacyOrchestrationRetirementPlan,
    before_staging: impl FnOnce() -> Result<()>,
) -> Result<()> {
    apply_legacy_orchestration_retirement_with_hooks(plan, before_staging, || Ok(()))
}

fn apply_legacy_orchestration_retirement_with_hooks(
    plan: &LegacyOrchestrationRetirementPlan,
    before_staging: impl FnOnce() -> Result<()>,
    before_mutation: impl FnOnce() -> Result<()>,
) -> Result<()> {
    for file in &plan.files {
        let current = read_regular_file_without_symlinks(&plan.root, &file.path)?;
        if current.as_deref() != Some(file.original.as_slice()) {
            return Err(Error::Config(format!(
                "legacy orchestration asset changed after preflight: {}",
                file.path.display()
            )));
        }
        validate_backup(&file.backup, &file.original)?;
    }

    before_staging()?;

    for file in &plan.files {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file.backup)
        {
            Ok(mut backup) => backup.write_all(&file.original).map_err(Error::Io)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                validate_backup(&file.backup, &file.original)?;
            }
            Err(error) => return Err(Error::Io(error)),
        }
    }
    for file in &plan.files {
        verify_retirement_source(&plan.root, file, "after backup staging")?;
    }

    before_mutation()?;

    for file in &plan.files {
        // Re-checked here, not just in the loop above: a whole-plan sweep leaves
        // a window in which an already-verified target is swapped while the
        // remaining targets are still being checked.
        verify_retirement_source(&plan.root, file, "before retirement")?;
        match &file.replacement {
            Some(replacement) => replace_retirement_target(&file.path, replacement)?,
            None => std::fs::remove_file(&file.path).map_err(Error::Io)?,
        }
    }
    Ok(())
}

// Retirement must never write *through* a symlink a racing process drops at the
// target. A sibling temp file plus rename replaces such a link instead of
// following it, which no amount of pre-write checking can guarantee on its own.
#[cfg(unix)]
fn replace_retirement_target(path: &Path, content: &[u8]) -> Result<()> {
    render::write_replacing(path, content, false)
}

#[cfg(not(unix))]
fn replace_retirement_target(path: &Path, content: &[u8]) -> Result<()> {
    std::fs::write(path, content).map_err(Error::Io)
}

fn verify_retirement_source(root: &Path, file: &LegacyRetirementFile, stage: &str) -> Result<()> {
    let current = read_regular_file_without_symlinks(root, &file.path)?;
    if current.as_deref() != Some(file.original.as_slice()) {
        return Err(Error::Config(format!(
            "legacy orchestration asset changed {stage}: {}",
            file.path.display()
        )));
    }
    validate_backup(&file.backup, &file.original)
}

fn remove_exact_legacy_section(original: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(original).ok()?;
    let legacy = builtin::LEGACY_STANDARDIZATION_AGENT_TEAMS;
    let matches = text
        .match_indices(legacy)
        .filter(|(start, matched)| {
            (*start == 0 || text.as_bytes().get(start.saturating_sub(1)) == Some(&b'\n'))
                && text
                    .get(start + matched.len()..)
                    .is_some_and(|remainder| remainder.is_empty() || remainder.starts_with("\n## "))
        })
        .map(|(start, _matched)| start)
        .collect::<Vec<_>>();
    let [start] = matches.as_slice() else {
        return None;
    };
    let end = start + legacy.len();
    let mut replacement = Vec::with_capacity(original.len() - legacy.len());
    replacement.extend_from_slice(&original[..*start]);
    replacement.extend_from_slice(&original[end..]);
    Some(replacement)
}

fn read_regular_file_without_symlinks(root: &Path, path: &Path) -> Result<Option<Vec<u8>>> {
    let relative = path.strip_prefix(root).map_err(|_| {
        Error::Config(format!(
            "legacy asset escaped project root: {}",
            path.display()
        ))
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Ok(None);
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(None),
            Ok(metadata) if current == path && metadata.is_file() => {
                return std::fs::read(path).map(Some).map_err(Error::Io);
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(None)
}

fn validate_backup(path: &Path, original: &[u8]) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && std::fs::read(path).map_err(Error::Io)? == original =>
        {
            Ok(())
        }
        Ok(_) => Err(Error::Config(format!(
            "legacy orchestration backup conflicts with retirement: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut extension = path
        .extension()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !extension.is_empty() {
        extension.push('.');
    }
    extension.push_str("bak");
    path.with_extension(extension)
}

struct RenderedStandardization {
    contract_content: String,
    agent_files: Vec<(PathBuf, String)>,
}

fn rendered_standardization(root: impl AsRef<Path>) -> Result<RenderedStandardization> {
    let template = standardization_template()?;
    let vars = default_vars(root.as_ref());
    rendered_contract(&template, root, vars)
}

fn rendered_contract_template(
    root: impl AsRef<Path>,
    template_name: &str,
    vars: BTreeMap<String, String>,
) -> Result<RenderedStandardization> {
    let template = contract_template(template_name)?;
    let mut merged = default_vars(root.as_ref());
    merged.extend(vars);
    rendered_contract(&template, root, merged)
}

fn rendered_contract(
    template: &Template,
    root: impl AsRef<Path>,
    vars: BTreeMap<String, String>,
) -> Result<RenderedStandardization> {
    let plan = render::plan(template, root.as_ref(), vars)?;
    let mut contract_content = None;
    let mut agent_files = Vec::new();
    for file in plan.files {
        let relative = file
            .path
            .strip_prefix(root.as_ref())
            .map_err(|_| {
                Error::Config(format!(
                    "template path escaped root: {}",
                    file.path.display()
                ))
            })?
            .to_path_buf();
        if !is_safe_relative_path(&relative) {
            return Err(Error::Config(format!(
                "template path is not a safe relative path: {}",
                relative.display()
            )));
        }
        if relative == Path::new(CONTRACT_PATH) {
            contract_content = Some(file.content);
        } else if relative.starts_with(AGENTS_DIR) {
            agent_files.push((relative, file.content));
        }
    }
    let contract_content = contract_content
        .ok_or_else(|| Error::Config("standardization template lacks CLAUDE.md".into()))?;
    Ok(RenderedStandardization {
        contract_content,
        agent_files,
    })
}

fn default_vars(root: &Path) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    let project_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("project");
    vars.insert("project_name".into(), project_name.into());
    vars.insert("license".into(), "MIT".into());
    vars.insert("language".into(), "Rust".into());
    vars.insert("framework".into(), String::new());
    vars.insert("target_platform".into(), "Linux / macOS / Windows".into());
    vars.insert("deployment".into(), "GitHub Actions".into());
    vars
}

fn parse_sections(input: &str) -> Vec<ParsedSection> {
    let mut headings = Vec::new();
    let mut offset = 0;
    for line in input.split_inclusive('\n') {
        if let Some((number, title)) = parse_section_heading(line) {
            headings.push((offset, number, title));
        }
        offset += line.len();
    }
    let mut sections = Vec::new();
    for (idx, (start, number, title)) in headings.iter().enumerate() {
        let end = headings
            .get(idx + 1)
            .map(|(next_start, _number, _title)| *next_start)
            .unwrap_or(input.len());
        let content = input[*start..end].trim_end().to_string();
        sections.push(ParsedSection {
            number: *number,
            title: title.clone(),
            content,
        });
    }
    sections
}

fn parse_section_heading(line: &str) -> Option<(u8, String)> {
    let trimmed = line.trim_end();
    let rest = trimmed.strip_prefix("## ")?;
    let (number, title) = rest.split_once('.')?;
    let number = number.trim().parse::<u8>().ok()?;
    if !(FIRST_MANAGED_SECTION..=LAST_MANAGED_SECTION).contains(&number) {
        return Some((number, title.trim().to_string()));
    }
    Some((number, title.trim().to_string()))
}

fn agent_file_path(file: &TemplateFile) -> Option<&str> {
    let path = Path::new(&file.path);
    if path.starts_with(AGENTS_DIR) && path.extension().and_then(|ext| ext.to_str()) == Some("md") {
        Some(&file.path)
    } else {
        None
    }
}

fn present_agent_names(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(AGENTS_DIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            names.push(agent_name(&path));
        }
    }
    names.sort();
    Ok(names)
}

fn agent_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn is_safe_relative_path(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rtrt-project-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn expected_sections_are_the_project_contract_set() {
        // Given the built-in project contract.
        let root = std::env::temp_dir();

        // When its managed sections are enumerated.
        let sections = expected_sections(&root).expect("sections");
        let numbers = sections
            .iter()
            .map(|(number, _title)| *number)
            .collect::<Vec<_>>();

        // Then only the non-orchestration project sections remain active.
        assert_eq!(numbers, vec![1, 2, 3, 4, 5, 6, 7]);
        assert!(
            numbers
                .iter()
                .all(|n| (FIRST_MANAGED_SECTION..=LAST_MANAGED_SECTION).contains(n))
        );
    }

    #[test]
    fn expected_agents_come_from_standardization_template() {
        // Given the built-in standardization template.
        // When its managed agents are enumerated.
        let agents = expected_agents().expect("agents");

        // Then non-orchestration agents remain and the orchestration agent is absent.
        assert_eq!(
            agents,
            vec![
                PathBuf::from(".claude/agents/explorer.md"),
                PathBuf::from(".claude/agents/code-reviewer.md"),
                PathBuf::from(".claude/agents/log-analyzer.md"),
            ]
        );
    }

    const LEGACY_SECTION: &str = r#"## 11. Agent Teams

| Agent | Owned Paths | Domain | Model |
|-------|-------------|--------|-------|
| tech-lead | All | orchestration, planning, integration | claude-opus-4-5 |
| explorer | All | read-only code discovery and symbol mapping | claude-opus-4-5 |
| code-reviewer | All | diff review, conventions, security, tests | claude-sonnet-4-5 |
| log-analyzer | logs, traces, CI output | failure diagnosis and root cause analysis | claude-sonnet-4-5 |
"#;

    const LEGACY_TECH_LEAD: &str = r#"---
name: tech-lead
description: Orchestrates cross-cutting work, assigns sub-agents, integrates results, and enforces conventions.
tools: Read, Bash, Glob, Grep, Edit, Write
model: claude-opus-4-5
---

Break down cross-cutting tasks, assign focused sub-agent work, integrate results, and enforce this repository's conventions. Keep changes scoped, resolve conflicts deliberately, and make verification explicit before handoff.
"#;

    fn write_legacy_assets(root: &Path) {
        std::fs::write(
            root.join(CONTRACT_PATH),
            format!("# demo\n\n## 1. Project Identity\n\nKeep me.\n\n{LEGACY_SECTION}"),
        )
        .unwrap();
        let agents = root.join(AGENTS_DIR);
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("tech-lead.md"), LEGACY_TECH_LEAD).unwrap();
        std::fs::write(agents.join("explorer.md"), "foreign explorer\n").unwrap();
    }

    #[test]
    fn exact_owned_legacy_orchestration_is_backed_up_and_removed() {
        // Given exact legacy orchestration assets and a non-orchestration agent.
        let root = TestDir::new("exact-retirement");
        write_legacy_assets(root.path());
        let original_contract = std::fs::read(root.path().join(CONTRACT_PATH)).unwrap();

        // When retirement is planned and applied.
        let plan = plan_legacy_orchestration_retirement(root.path()).unwrap();
        apply_legacy_orchestration_retirement(&plan).unwrap();

        // Then owned bytes are backed up and retired without touching other content.
        let contract = std::fs::read_to_string(root.path().join(CONTRACT_PATH)).unwrap();
        assert!(!contract.contains("## 11. Agent Teams"));
        assert!(contract.contains("Keep me."));
        assert_eq!(
            std::fs::read(root.path().join("CLAUDE.md.bak")).unwrap(),
            original_contract
        );
        assert!(!root.path().join(AGENTS_DIR).join("tech-lead.md").exists());
        assert_eq!(
            std::fs::read(root.path().join(AGENTS_DIR).join("tech-lead.md.bak")).unwrap(),
            LEGACY_TECH_LEAD.as_bytes()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join(AGENTS_DIR).join("explorer.md")).unwrap(),
            "foreign explorer\n"
        );
    }

    #[test]
    fn modified_legacy_orchestration_is_preserved() {
        // Given legacy-shaped assets whose bytes were modified.
        let root = TestDir::new("modified-preserved");
        std::fs::write(
            root.path().join(CONTRACT_PATH),
            LEGACY_SECTION.replace("claude-opus-4-5", "custom-model"),
        )
        .unwrap();
        let agents = root.path().join(AGENTS_DIR);
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("tech-lead.md"),
            LEGACY_TECH_LEAD.replace("Break down", "Custom: break down"),
        )
        .unwrap();

        // When retirement is planned and applied.
        let plan = plan_legacy_orchestration_retirement(root.path()).unwrap();
        apply_legacy_orchestration_retirement(&plan).unwrap();

        // Then foreign bytes and names are preserved without backups.
        assert!(root.path().join(CONTRACT_PATH).exists());
        assert!(agents.join("tech-lead.md").exists());
        assert!(!root.path().join("CLAUDE.md.bak").exists());
        assert!(!agents.join("tech-lead.md.bak").exists());
    }

    #[test]
    fn legacy_section_with_appended_foreign_bytes_is_preserved() {
        // Given exact legacy bytes followed by unheaded project-owned content.
        let root = TestDir::new("appended-foreign-preserved");
        let contract = format!("{LEGACY_SECTION}\nProject-specific ownership notes.\n");
        std::fs::write(root.path().join(CONTRACT_PATH), &contract).unwrap();

        // When retirement is planned and applied.
        let plan = plan_legacy_orchestration_retirement(root.path()).unwrap();
        apply_legacy_orchestration_retirement(&plan).unwrap();

        // Then the modified section is preserved byte-for-byte.
        assert_eq!(
            std::fs::read_to_string(root.path().join(CONTRACT_PATH)).unwrap(),
            contract
        );
        assert!(!root.path().join("CLAUDE.md.bak").exists());
    }

    #[test]
    fn conflicting_backup_fails_before_any_legacy_deletion() {
        // Given two exact-owned assets but a conflicting backup for the second one.
        let root = TestDir::new("backup-conflict");
        write_legacy_assets(root.path());
        let contract_before = std::fs::read(root.path().join(CONTRACT_PATH)).unwrap();
        let agent = root.path().join(AGENTS_DIR).join("tech-lead.md");
        std::fs::write(agent.with_extension("md.bak"), "existing foreign backup\n").unwrap();

        // When retirement is planned.
        let error = plan_legacy_orchestration_retirement(root.path()).unwrap_err();

        // Then it fails closed before either owned asset is changed.
        assert!(error.to_string().contains("backup"));
        assert_eq!(
            std::fs::read(root.path().join(CONTRACT_PATH)).unwrap(),
            contract_before
        );
        assert_eq!(std::fs::read_to_string(agent).unwrap(), LEGACY_TECH_LEAD);
        assert!(!root.path().join("CLAUDE.md.bak").exists());
    }

    #[test]
    fn backup_created_after_preflight_is_not_overwritten() {
        // Given an exact-owned retirement plan and a conflicting backup created after preflight.
        let root = TestDir::new("late-backup-conflict");
        write_legacy_assets(root.path());
        let contract = root.path().join(CONTRACT_PATH);
        let agent = root.path().join(AGENTS_DIR).join("tech-lead.md");
        let contract_before = std::fs::read(&contract).unwrap();
        let agent_before = std::fs::read(&agent).unwrap();
        let backup = contract.with_extension("md.bak");
        let plan = plan_legacy_orchestration_retirement(root.path()).unwrap();

        // When a conflicting regular backup wins the race immediately before staging.
        let error = apply_legacy_orchestration_retirement_with_staging_hook(&plan, || {
            std::fs::write(&backup, b"late foreign backup\n").map_err(Error::Io)
        })
        .unwrap_err();

        // Then the backup and every original source remain byte-exact.
        assert!(error.to_string().contains("backup"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"late foreign backup\n");
        assert_eq!(std::fs::read(contract).unwrap(), contract_before);
        assert_eq!(std::fs::read(agent).unwrap(), agent_before);
    }

    #[test]
    fn source_swapped_after_backup_staging_is_not_retired() {
        // Given an exact-owned retirement plan whose backups have already been staged.
        let root = TestDir::new("late-source-swap");
        write_legacy_assets(root.path());
        let contract = root.path().join(CONTRACT_PATH);
        let agent = root.path().join(AGENTS_DIR).join("tech-lead.md");
        let agent_before = std::fs::read(&agent).unwrap();
        let plan = plan_legacy_orchestration_retirement(root.path()).unwrap();

        // When the first source is swapped after the whole-plan sweep passed.
        let error = apply_legacy_orchestration_retirement_with_hooks(
            &plan,
            || Ok(()),
            || std::fs::write(&contract, b"swapped after staging\n").map_err(Error::Io),
        )
        .unwrap_err();

        // Then nothing is retired and the swapped bytes are left alone.
        assert!(error.to_string().contains("before retirement"));
        assert_eq!(
            std::fs::read(&contract).unwrap(),
            b"swapped after staging\n"
        );
        assert_eq!(std::fs::read(&agent).unwrap(), agent_before);
    }

    #[cfg(unix)]
    #[test]
    fn source_symlinked_immediately_before_retirement_is_not_written_through() {
        use std::os::unix::fs::symlink;

        // Given a staged retirement plan and an outside file the attacker targets.
        let root = TestDir::new("late-source-symlink");
        write_legacy_assets(root.path());
        let contract = root.path().join(CONTRACT_PATH);
        let outside = root.path().join("victim-secret");
        std::fs::write(&outside, b"victim secret\n").unwrap();
        let plan = plan_legacy_orchestration_retirement(root.path()).unwrap();

        // When the contract is swapped for a symlink after the whole-plan sweep.
        let result = apply_legacy_orchestration_retirement_with_hooks(
            &plan,
            || Ok(()),
            || {
                std::fs::remove_file(&contract).map_err(Error::Io)?;
                symlink(&outside, &contract).map_err(Error::Io)
            },
        );

        // Then the outside file keeps its bytes, whatever the retirement decided.
        assert_eq!(std::fs::read(&outside).unwrap(), b"victim secret\n");
        if result.is_ok() {
            assert!(
                !std::fs::symlink_metadata(&contract)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn backup_symlink_created_after_preflight_is_not_followed() {
        use std::os::unix::fs::symlink;

        // Given an exact-owned retirement plan and an external symlink target.
        let root = TestDir::new("late-backup-symlink");
        write_legacy_assets(root.path());
        let contract = root.path().join(CONTRACT_PATH);
        let agent = root.path().join(AGENTS_DIR).join("tech-lead.md");
        let contract_before = std::fs::read(&contract).unwrap();
        let agent_before = std::fs::read(&agent).unwrap();
        let backup = contract.with_extension("md.bak");
        let target = root.path().join("foreign-backup-target");
        std::fs::write(&target, b"foreign target\n").unwrap();
        let plan = plan_legacy_orchestration_retirement(root.path()).unwrap();

        // When a symlink wins the race immediately before staging.
        let error = apply_legacy_orchestration_retirement_with_staging_hook(&plan, || {
            symlink(&target, &backup).map_err(Error::Io)
        })
        .unwrap_err();

        // Then the link, target, and every original source remain untouched.
        assert!(error.to_string().contains("backup"));
        assert!(
            std::fs::symlink_metadata(&backup)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(target).unwrap(), b"foreign target\n");
        assert_eq!(std::fs::read(contract).unwrap(), contract_before);
        assert_eq!(std::fs::read(agent).unwrap(), agent_before);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_legacy_agent_is_preserved() {
        use std::os::unix::fs::symlink;

        // Given a tech-lead path symlinked to exact legacy bytes outside the agents directory.
        let root = TestDir::new("symlink-preserved");
        let target = root.path().join("foreign-tech-lead.md");
        std::fs::write(&target, LEGACY_TECH_LEAD).unwrap();
        let agents = root.path().join(AGENTS_DIR);
        std::fs::create_dir_all(&agents).unwrap();
        let link = agents.join("tech-lead.md");
        symlink(&target, &link).unwrap();

        // When retirement is planned and applied.
        let plan = plan_legacy_orchestration_retirement(root.path()).unwrap();
        apply_legacy_orchestration_retirement(&plan).unwrap();

        // Then the link and its target remain untouched.
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), LEGACY_TECH_LEAD);
    }
}
