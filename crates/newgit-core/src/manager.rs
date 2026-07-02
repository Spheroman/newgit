use camino::Utf8Path;
use camino::Utf8PathBuf;

use crate::branch::{BranchInstance, branch_slug, validate_name};
use crate::config::ProjectConfig;
use crate::error::{NewgitError, Result};
use crate::materializer::{Materializer, RealDirMaterializer};
use crate::source::GitSource;
use crate::store::MetadataStore;

/// Orchestrates branch-instance lifecycle against one store.
#[derive(Debug)]
pub struct BranchManager {
    store: MetadataStore,
    config: ProjectConfig,
    source: GitSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnOutcome {
    pub branch: BranchInstance,
    pub record_path: Utf8PathBuf,
    /// False when the instance attached to a pre-existing source branch.
    pub created_source_branch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceReport {
    pub branch: BranchInstance,
    pub workspace_exists: bool,
    /// Live HEAD of the workspace clone, when it can be read.
    pub live_rev: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveOutcome {
    pub branch: BranchInstance,
    pub archived_record: Utf8PathBuf,
}

impl BranchManager {
    pub fn open(store: MetadataStore) -> Result<Self> {
        store.ensure_initialized()?;
        let config = store.load_config()?;
        let source = GitSource::open(&store.paths().project_root, config.project.source)?;
        Ok(Self {
            store,
            config,
            source,
        })
    }

    pub fn store(&self) -> &MetadataStore {
        &self.store
    }

    pub fn spawn(&self, name: &str, from: Option<&str>) -> Result<SpawnOutcome> {
        validate_name(name)?;

        let slug = branch_slug(name);
        let record_path = self.store.branch_record_path(&slug);
        if record_path.exists() {
            return Err(NewgitError::BranchInstanceExists {
                name: name.to_owned(),
                path: record_path,
            });
        }

        let created_source_branch = if self.source.branch_exists(name)? {
            if let Some(base) = from {
                return Err(NewgitError::Unsupported(format!(
                    "source branch `{name}` already exists; `--from {base}` only applies when \
                     creating a new branch"
                )));
            }
            false
        } else {
            let base = from.unwrap_or("HEAD");
            let base_rev = self.source.rev_parse(base)?;
            self.source.create_branch(name, &base_rev)?;
            true
        };

        let source_rev = self.source.rev_parse(&format!("refs/heads/{name}"))?;
        let workspace_path = self
            .config
            .workspace_root(&self.store.paths().project_root)
            .join(&slug);
        let branch = BranchInstance::new(name, name, source_rev, workspace_path)?;

        RealDirMaterializer.materialize(&self.source, &branch)?;
        let record_path = self.store.create_branch_record(&branch)?;

        Ok(SpawnOutcome {
            branch,
            record_path,
            created_source_branch,
        })
    }

    pub fn statuses(&self) -> Result<Vec<InstanceReport>> {
        self.store
            .load_branches()?
            .into_iter()
            .map(|branch| {
                let workspace_exists = branch.workspace_path.is_dir();
                let live_rev = workspace_exists
                    .then(|| GitSource::workspace_short_head(&branch.workspace_path).ok())
                    .flatten();
                Ok(InstanceReport {
                    branch,
                    workspace_exists,
                    live_rev,
                })
            })
            .collect()
    }

    /// Deletes the workspace (plain `rm -rf`; clones have no registration)
    /// and archives the binding record. The source branch in the store is
    /// kept — removal disposes of the workspace, not the history.
    pub fn remove(&self, name: &str, cwd: &Utf8Path) -> Result<RemoveOutcome> {
        let branch = self.store.find_branch(name)?;

        if cwd.starts_with(&branch.workspace_path) {
            return Err(NewgitError::Unsupported(format!(
                "the current directory is inside the workspace of `{}`; step out of it before \
                 removing",
                branch.name
            )));
        }

        RealDirMaterializer.remove(&branch)?;
        let archived_record = self.store.archive_branch_record(&branch)?;

        Ok(RemoveOutcome {
            branch,
            archived_record,
        })
    }
}
