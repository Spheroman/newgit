use std::collections::BTreeSet;

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
    exclude_paths(
        workspace,
        "tracker-owned paths — these lanes own this content",
        paths,
    )
}

/// The same, for the trees resources build (`[identity] produces`).
///
/// Derived content is not source, and a produced tree is the largest thing
/// in a workspace by orders of magnitude. Without this, an instance whose
/// project has no `node_modules` rule of its own would have its whole
/// install swept into source history by the `git add -A` a checkpoint runs —
/// and unlike a tracker lane, nothing else keeps it out.
pub fn exclude_produced_paths(workspace: &Utf8Path, paths: &[Utf8PathBuf]) -> Result<()> {
    exclude_paths(
        workspace,
        "resource-produced trees — derived from an identity, never source",
        paths,
    )
}

/// Idempotent: paths already excluded are skipped, and nothing is written
/// when they all are. Callers re-run this to re-sync a workspace that
/// predates a definition change, and an append-always version would grow the
/// file a block at a time on every checkpoint.
fn exclude_paths(workspace: &Utf8Path, note: &str, paths: &[Utf8PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let exclude = workspace.join(".git/info/exclude");
    if let Some(parent) = exclude.parent() {
        create_dir_all(parent)?;
    }
    let mut contents = std::fs::read_to_string(&exclude).unwrap_or_default();
    let existing: BTreeSet<&str> = contents.lines().map(str::trim).collect();
    let wanted: Vec<String> = paths
        .iter()
        .map(|path| format!("/{path}"))
        .filter(|pattern| !existing.contains(pattern.as_str()))
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(&format!("\n# newgit: {note}\n"));
    for pattern in wanted {
        contents.push_str(&format!("{pattern}\n"));
    }
    std::fs::write(&exclude, contents).map_err(|source| NewgitError::io(exclude, source))
}

/// Reverse [`exclude_tracker_paths`] for one workspace: drop this tracker's
/// paths from `.git/info/exclude`, leaving every other tracker's lines (and
/// the shared header comment) alone. Returns whether anything changed, so a
/// caller iterating many workspaces can report only the ones it touched.
pub fn unexclude_tracker_paths(workspace: &Utf8Path, paths: &[Utf8PathBuf]) -> Result<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    let exclude = workspace.join(".git/info/exclude");
    let Ok(contents) = std::fs::read_to_string(&exclude) else {
        return Ok(false);
    };
    let wanted: Vec<String> = paths.iter().map(|path| format!("/{path}")).collect();

    let mut changed = false;
    let kept: Vec<&str> = contents
        .lines()
        .filter(|line| {
            let drop = wanted.iter().any(|pattern| pattern == line);
            changed |= drop;
            !drop
        })
        .collect();
    if !changed {
        return Ok(false);
    }

    let mut updated = kept.join("\n");
    if !updated.is_empty() {
        updated.push('\n');
    }
    std::fs::write(&exclude, updated).map_err(|source| NewgitError::io(exclude, source))?;
    Ok(true)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unexclude_removes_only_the_named_trackers_lines() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let env = Utf8PathBuf::from(".env.local");
        let secret = Utf8PathBuf::from("secrets/token");
        exclude_tracker_paths(&workspace, &[env.clone(), secret.clone()]).expect("exclude");

        let changed =
            unexclude_tracker_paths(&workspace, std::slice::from_ref(&env)).expect("unexclude");
        assert!(changed);

        let contents =
            std::fs::read_to_string(workspace.join(".git/info/exclude")).expect("read exclude");
        assert!(
            !contents.contains("/.env.local"),
            "the removed tracker's path must be gone"
        );
        assert!(
            contents.contains("/secrets/token"),
            "another tracker's path in the same shared block must survive"
        );

        // Nothing left to remove the second time.
        assert!(!unexclude_tracker_paths(&workspace, &[env]).expect("unexclude again"));
    }

    #[test]
    fn unexclude_is_a_no_op_when_there_is_no_exclude_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let changed = unexclude_tracker_paths(&workspace, &[Utf8PathBuf::from(".env")])
            .expect("no exclude file yet");
        assert!(!changed);
    }
}
