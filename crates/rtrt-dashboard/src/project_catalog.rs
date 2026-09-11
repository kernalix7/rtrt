//! Bounded, read-only discovery of machine-wide project stores.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rtrt_core::ProjectIdentity;
use rtrt_memory::{
    MemoryStore, ProjectStoreInspection, inspect_project_store_for_identity_in,
    inspect_project_store_in,
};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::state::ProjectContext;

/// Maximum direct children considered during one refresh. Discovery never
/// descends into a child.
pub(crate) const DISCOVERY_CAP: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticCategory {
    Unavailable,
    UnsafePath,
    MissingRoot,
    IdentityMismatch,
    Incompatible,
    Collision,
    DiscoveryLimit,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CatalogProjectView {
    pub(crate) slug: String,
    pub(crate) label: Option<String>,
    pub(crate) memory_root: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) available: bool,
    pub(crate) diagnostic: Option<DiagnosticCategory>,
    pub(crate) memory_count: Option<usize>,
    pub(crate) database_schema_version: Option<i64>,
    pub(crate) database_schema_compatible: Option<bool>,
    pub(crate) integrity_ok: Option<bool>,
    pub(crate) embeddings_enabled: Option<bool>,
    pub(crate) security_profile: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ProjectCatalog {
    home: PathBuf,
    projects_root: PathBuf,
    inner: Arc<RwLock<CatalogSnapshot>>,
}

#[derive(Default)]
struct CatalogSnapshot {
    contexts: BTreeMap<String, Arc<ProjectContext>>,
    views: Vec<CatalogProjectView>,
    generation: u64,
}

impl ProjectCatalog {
    pub(crate) fn new(home: PathBuf, projects_root: PathBuf) -> Self {
        Self {
            home,
            projects_root,
            inner: Arc::new(RwLock::new(CatalogSnapshot::default())),
        }
    }

    pub(crate) fn refresh(&self) {
        let previous = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let old_contexts = previous.contexts.clone();
        let generation = previous.generation.saturating_add(1);
        drop(previous);

        let (mut views, candidates) = discover_children(&self.projects_root);
        let mut contexts = BTreeMap::new();
        let mut seen = HashSet::new();

        for candidate in candidates {
            let candidate_name = candidate
                .file_name()
                .and_then(|value| value.to_str())
                .filter(|value| valid_slug(value))
                .unwrap_or("unavailable")
                .to_string();
            match inspect_candidate(&self.home, &self.projects_root, &candidate) {
                Ok((inspection, identity, store)) => {
                    let slug = identity.slug().to_string();
                    if !seen.insert(slug.clone()) {
                        contexts.remove(&slug);
                        views.push(unavailable_view(slug, DiagnosticCategory::Collision));
                        continue;
                    }
                    let context = old_contexts.get(&slug).cloned().unwrap_or_else(|| {
                        Arc::new(ProjectContext::new(
                            Arc::new(identity),
                            Arc::new(Mutex::new(store)),
                            inspection.database_path.clone(),
                        ))
                    });
                    let memory_count = context
                        .memory
                        .try_lock()
                        .ok()
                        .and_then(|store| store.count_by_project(&slug).ok());
                    contexts.insert(slug.clone(), context);
                    views.push(CatalogProjectView {
                        slug,
                        label: Some(inspection.label),
                        memory_root: Some(inspection.memory_root.to_string_lossy().into_owned()),
                        path: Some(inspection.database_path.to_string_lossy().into_owned()),
                        available: true,
                        diagnostic: None,
                        memory_count,
                        database_schema_version: Some(inspection.database_schema_version),
                        database_schema_compatible: Some(inspection.database_schema_compatible),
                        integrity_ok: Some(inspection.integrity_ok),
                        embeddings_enabled: None,
                        security_profile: None,
                    });
                }
                Err(category) => {
                    // A transient invalid rediscovery must not evict a context
                    // already verified and in active use.
                    if let Some(context) = old_contexts.get(&candidate_name) {
                        contexts.insert(candidate_name.clone(), context.clone());
                        let memory_count = context
                            .memory
                            .try_lock()
                            .ok()
                            .and_then(|store| store.count_by_project(&candidate_name).ok());
                        views.push(CatalogProjectView {
                            slug: candidate_name,
                            label: Some(context.project.label().to_string()),
                            memory_root: Some(
                                context.project.memory_root().to_string_lossy().into_owned(),
                            ),
                            path: Some(context.memory_path.to_string_lossy().into_owned()),
                            available: true,
                            diagnostic: None,
                            memory_count,
                            database_schema_version: None,
                            database_schema_compatible: None,
                            integrity_ok: Some(true),
                            embeddings_enabled: None,
                            security_profile: None,
                        });
                        continue;
                    }
                    views.push(unavailable_view(candidate_name, category));
                }
            }
        }
        views.sort_by(|a, b| {
            a.slug
                .cmp(&b.slug)
                .then(a.available.cmp(&b.available).reverse())
        });
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *guard = CatalogSnapshot {
            contexts,
            views,
            generation,
        };
    }

    pub(crate) fn get(&self, slug: &str) -> Option<Arc<ProjectContext>> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contexts
            .get(slug)
            .cloned()
    }

    pub(crate) fn contexts(&self) -> Vec<Arc<ProjectContext>> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contexts
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn views(&self) -> Vec<CatalogProjectView> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .views
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn from_context(home: PathBuf, context: Arc<ProjectContext>) -> Self {
        let catalog = Self::new(home.clone(), home.join(".rtrt/projects"));
        let slug = context.project.slug().to_string();
        let view = CatalogProjectView {
            slug: slug.clone(),
            label: Some(context.project.label().to_string()),
            memory_root: Some(context.project.memory_root().to_string_lossy().into_owned()),
            path: Some(context.memory_path.to_string_lossy().into_owned()),
            available: true,
            diagnostic: None,
            memory_count: Some(0),
            database_schema_version: None,
            database_schema_compatible: None,
            integrity_ok: Some(true),
            embeddings_enabled: None,
            security_profile: None,
        };
        *catalog.inner.write().unwrap() = CatalogSnapshot {
            contexts: BTreeMap::from([(slug, context)]),
            views: vec![view],
            generation: 1,
        };
        catalog
    }
}

fn discover_children(root: &Path) -> (Vec<CatalogProjectView>, Vec<PathBuf>) {
    let mut diagnostics = Vec::new();
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => metadata,
        _ => {
            diagnostics.push(unavailable_view(
                "catalog".into(),
                DiagnosticCategory::UnsafePath,
            ));
            return (diagnostics, Vec::new());
        }
    };
    let _ = metadata;
    let Ok(entries) = std::fs::read_dir(root) else {
        diagnostics.push(unavailable_view(
            "catalog".into(),
            DiagnosticCategory::Unavailable,
        ));
        return (diagnostics, Vec::new());
    };
    let mut candidates = Vec::new();
    for entry in entries.take(DISCOVERY_CAP + 1) {
        match entry {
            Ok(entry) if candidates.len() < DISCOVERY_CAP => candidates.push(entry.path()),
            Ok(_) => diagnostics.push(unavailable_view(
                "discovery_limit".into(),
                DiagnosticCategory::DiscoveryLimit,
            )),
            Err(_) => diagnostics.push(unavailable_view(
                "unreadable".into(),
                DiagnosticCategory::Unavailable,
            )),
        }
    }
    candidates.sort();
    (diagnostics, candidates)
}

fn inspect_candidate(
    home: &Path,
    projects_root: &Path,
    candidate: &Path,
) -> Result<(ProjectStoreInspection, ProjectIdentity, MemoryStore), DiagnosticCategory> {
    let metadata =
        std::fs::symlink_metadata(candidate).map_err(|_| DiagnosticCategory::Unavailable)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DiagnosticCategory::UnsafePath);
    }
    let first = inspect_project_store_in(projects_root, candidate)
        .map_err(|_| DiagnosticCategory::Unavailable)?;
    if !first.database_schema_compatible || !first.integrity_ok {
        return Err(DiagnosticCategory::Incompatible);
    }
    let root_meta = std::fs::symlink_metadata(&first.memory_root)
        .map_err(|_| DiagnosticCategory::MissingRoot)?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Err(DiagnosticCategory::MissingRoot);
    }
    let identity = ProjectIdentity::derive(&first.memory_root)
        .map_err(|_| DiagnosticCategory::IdentityMismatch)?;
    let verified = inspect_project_store_for_identity_in(projects_root, candidate, &identity)
        .map_err(|_| DiagnosticCategory::IdentityMismatch)?;
    let store = MemoryStore::open_project_in(&identity, home)
        .map_err(|_| DiagnosticCategory::Unavailable)?;
    Ok((verified, identity, store))
}

fn unavailable_view(slug: String, diagnostic: DiagnosticCategory) -> CatalogProjectView {
    CatalogProjectView {
        slug,
        label: None,
        memory_root: None,
        path: None,
        available: false,
        diagnostic: Some(diagnostic),
        memory_count: None,
        database_schema_version: None,
        database_schema_compatible: None,
        integrity_ok: None,
        embeddings_enabled: None,
        security_profile: None,
    }
}

pub(crate) fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    /// macOS reaches the system temp dir through `/var -> /private/var`, and
    /// project identity derives from the canonical path, so a raw handle path
    /// makes the derived slug disagree with the home the catalog scans.
    struct CanonicalTempDir {
        _guard: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    impl CanonicalTempDir {
        fn new() -> Self {
            let guard = tempfile::tempdir().unwrap();
            let path = std::fs::canonicalize(guard.path()).unwrap();
            Self {
                _guard: guard,
                path,
            }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    #[test]
    fn discovers_same_basename_projects_as_distinct_canonical_slugs() {
        use std::os::unix::fs::PermissionsExt;

        let home = CanonicalTempDir::new();
        std::fs::create_dir_all(home.path().join(".rtrt/projects")).unwrap();
        for path in [
            home.path().join(".rtrt"),
            home.path().join(".rtrt/projects"),
        ] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let first_root = home.path().join("one/same");
        let second_root = home.path().join("two/same");
        std::fs::create_dir_all(first_root.join(".git")).unwrap();
        std::fs::create_dir_all(second_root.join(".git")).unwrap();
        let first = ProjectIdentity::derive(&first_root).unwrap();
        let second = ProjectIdentity::derive(&second_root).unwrap();
        assert_ne!(first.slug(), second.slug());
        {
            let store = MemoryStore::open_project_in(&first, home.path()).unwrap();
            store.save(first.slug(), "note", "first").unwrap();
        }
        {
            let store = MemoryStore::open_project_in(&second, home.path()).unwrap();
            store.save(second.slug(), "note", "second").unwrap();
        }

        let catalog = ProjectCatalog::new(
            home.path().to_path_buf(),
            home.path().join(".rtrt/projects"),
        );
        catalog.refresh();
        assert!(catalog.get(first.slug()).is_some());
        assert!(catalog.get(second.slug()).is_some());
        assert_eq!(catalog.contexts().len(), 2);
    }
}
