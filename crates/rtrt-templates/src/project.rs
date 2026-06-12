//! Reusable project-standardization lifecycle helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use rtrt_core::{Error, Result};

use crate::{Template, TemplateFile, builtin, render};

/// First managed standardization section number.
pub const FIRST_MANAGED_SECTION: u8 = 1;

/// Last managed standardization section number.
pub const LAST_MANAGED_SECTION: u8 = 11;

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
    let root = validate_project_path(root)?;
    let rendered = rendered_standardization(&root)?;
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
    let root = validate_project_path(root)?;
    let rendered = rendered_standardization(&root)?;
    let inspection = inspect_project(&root)?;
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

struct RenderedStandardization {
    contract_content: String,
    agent_files: Vec<(PathBuf, String)>,
}

fn rendered_standardization(root: impl AsRef<Path>) -> Result<RenderedStandardization> {
    let template = standardization_template()?;
    let vars = default_vars(root.as_ref());
    let plan = render::plan(&template, root.as_ref(), vars)?;
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

    #[test]
    fn expected_sections_cover_full_contract_range() {
        let root = std::env::temp_dir();
        let sections = expected_sections(&root).expect("sections");
        let numbers = sections
            .iter()
            .map(|(number, _title)| *number)
            .collect::<Vec<_>>();
        assert_eq!(
            numbers,
            (FIRST_MANAGED_SECTION..=LAST_MANAGED_SECTION).collect::<Vec<_>>()
        );
    }

    #[test]
    fn expected_agents_come_from_standardization_template() {
        let agents = expected_agents().expect("agents");
        assert!(agents.contains(&PathBuf::from(".claude/agents/explorer.md")));
        assert!(agents.contains(&PathBuf::from(".claude/agents/code-reviewer.md")));
        assert!(agents.contains(&PathBuf::from(".claude/agents/log-analyzer.md")));
        assert!(agents.contains(&PathBuf::from(".claude/agents/tech-lead.md")));
    }
}
