use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::branch::BranchInstance;
use crate::error::{NewgitError, Result};
use crate::source::GitSource;

/// The materializer contract: a workspace presents a full, real, verifiable
/// Git repo at the binding record's revision. Nothing outside this boundary
/// may care how that presentation is produced — this implementation clones;
/// a future one may project.
pub trait Materializer {
    fn materialize(&self, source: &GitSource, branch: &BranchInstance) -> Result<()>;
    fn remove(&self, branch: &BranchInstance) -> Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RealDirMaterializer;

impl Materializer for RealDirMaterializer {
    fn materialize(&self, source: &GitSource, branch: &BranchInstance) -> Result<()> {
        if branch.workspace_path.exists() {
            return Err(NewgitError::WorkspaceExists(branch.workspace_path.clone()));
        }
        if let Some(parent) = branch.workspace_path.parent() {
            create_dir_all(parent)?;
        }
        source.clone_to(&branch.source_ref, &branch.workspace_path)?;
        write_workspace_marker(source.root(), branch)
    }

    fn remove(&self, branch: &BranchInstance) -> Result<()> {
        if branch.workspace_path.exists() {
            std::fs::remove_dir_all(&branch.workspace_path)
                .map_err(|source| NewgitError::io(branch.workspace_path.clone(), source))?;
        }
        Ok(())
    }
}

/// Gitignored marker inside a workspace pointing back at the store, so
/// newgit commands run from inside a workspace find the real metadata and
/// know which instance they are standing in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceMarker {
    pub branch: String,
    pub store_root: Utf8PathBuf,
}

pub fn workspace_marker_path(workspace: &Utf8Path) -> Utf8PathBuf {
    workspace.join(".newgit/local/instance.toml")
}

fn write_workspace_marker(store_root: &Utf8Path, branch: &BranchInstance) -> Result<()> {
    let marker = WorkspaceMarker {
        branch: branch.name.clone(),
        store_root: store_root.to_path_buf(),
    };
    let path = workspace_marker_path(&branch.workspace_path);
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(&marker).map_err(|source| NewgitError::TomlWrite {
        label: "workspace marker".to_owned(),
        source,
    })?;
    std::fs::write(&path, contents).map_err(|source| NewgitError::io(path, source))
}

pub fn create_dir_all(path: &Utf8Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| NewgitError::io(path.to_path_buf(), source))
}
