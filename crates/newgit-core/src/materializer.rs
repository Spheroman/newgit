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

/// Keep tracker-owned paths out of the workspace clone's Git by writing them
/// to `.git/info/exclude`.
///
/// `newgit tracker track` appends to the store's `.gitignore`, but that edit
/// is uncommitted until the user commits it — so a clone would not inherit
/// the rule, and a lane's content would sit in the workspace as ordinary
/// untracked files that `git add -A` sweeps into source history. Audience
/// only keeps content out of Git *by construction* if the construction
/// reaches every workspace.
///
/// `info/exclude` rather than the workspace's `.gitignore`: the latter is
/// tracked content owned by source, and newgit does not rewrite the user's
/// committed files. This is Git's own per-clone local-ignore mechanism, and
/// there is no plumbing command that writes it.
pub fn exclude_tracker_paths(workspace: &Utf8Path, paths: &[Utf8PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let exclude = workspace.join(".git/info/exclude");
    if let Some(parent) = exclude.parent() {
        create_dir_all(parent)?;
    }
    let mut contents = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str("\n# newgit: tracker-owned paths — these lanes own this content\n");
    for path in paths {
        contents.push_str(&format!("/{path}\n"));
    }
    std::fs::write(&exclude, contents).map_err(|source| NewgitError::io(exclude, source))
}

fn write_workspace_marker(store_root: &Utf8Path, branch: &BranchInstance) -> Result<()> {
    let marker = WorkspaceMarker {
        branch: branch.name.clone(),
        store_root: store_root.to_path_buf(),
    };
    let path = workspace_marker_path(&branch.workspace_path);
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
        // Self-ignoring: the marker must never show up in `git status` or be
        // committable, even when the project has no committed .newgit rules.
        let gitignore = parent.join(".gitignore");
        std::fs::write(&gitignore, "*\n").map_err(|source| NewgitError::io(gitignore, source))?;
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
